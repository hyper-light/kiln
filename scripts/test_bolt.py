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

    def test_g11_s5_a_link_in_a_response_file_is_a_link(self) -> None:
        rsp = self.dir / "args.rsp"
        rsp.write_text("-target\nx86_64-linux-musl\na.o\n-o\nout\n")
        r = self.relocs("cc", f"@{rsp}")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.calls()), 2, "run, then run again with its relocations")
        self.assertTrue(self.calls()[1].endswith("--emit-relocs --build-id=sha1"))
        rsp.write_text("-c\nx.c\n-o\nx.o\n")
        self.log.unlink()
        r = self.relocs("cc", f"@{rsp}")
        self.assertEqual((r.returncode, len(self.calls())), (0, 1), "a compile in a response file runs as it is")

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

    def test_g11_s5_an_order_lays_out_the_link_run_again(self) -> None:
        order = self.dir / "kiln.order"
        order.write_text("hot\nwarm\n")
        r = self.relocs("cc", "a.o", "-o", "out", KILN_LINK_ORDER=str(order))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls()[0], "1|cc a.o -o out", "zig's own link takes no order: it refuses LLD's options")
        self.assertTrue(
            self.calls()[1].endswith(f"--emit-relocs --build-id=sha1 --symbol-ordering-file={order} --no-warn-symbol-ordering"),
            self.calls(),
        )
        # A compile has nothing to order: it runs as it is, the order unread.
        self.log.unlink()
        r = self.relocs("cc", "-c", "x.c", "-o", "x.o", KILN_LINK_ORDER=str(self.dir / "missing.order"))
        self.assertEqual((r.returncode, self.calls()), (0, ["|cc -c x.c -o x.o"]))

    def test_g11_s5_an_order_that_isnt_there_stops_the_link_before_zig_runs(self) -> None:
        for order in (self.dir / "missing.order", self.dir / "empty.order"):
            if order.name == "empty.order":
                order.write_text("")
            self.log.unlink(missing_ok=True)
            r = self.relocs("cc", "a.o", "-o", "out", KILN_LINK_ORDER=str(order))
            self.assertEqual(r.returncode, 1, order)
            self.assertIn("names no order", r.stderr)
            self.assertEqual(self.calls(), [], "zig never linked")


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


def build_id(binary: Path) -> str:
    for l in run(["readelf", "-n", str(binary)]).stdout.splitlines():
        if l.split()[:2] == ["Build", "ID:"]:
            return l.split()[2]
    return ""


@unittest.skipUnless(shutil.which("cc") and shutil.which("readelf") and shutil.which("strip"), "takes cc, readelf and strip")
class Order(unittest.TestCase):
    """bolt.sh order: the functions profiles saw run, hottest first, for LLD's --symbol-ordering-file (no BOLT)."""

    def setUp(self) -> None:
        self.dir = Path(tempfile.mkdtemp())
        src = self.dir / "m.c"
        src.write_text(
            "__attribute__((noinline)) long hot(long n) { long s = 0; for (long i = 0; i < n; i++) s += i % 7; return s; }\n"
            "__attribute__((noinline)) long cold(long n) { return n * 3; }\n"
            "int main(int c, char **v) { (void)v; return (int)(c > 5 ? cold(c) : hot(100000000)) & 1; }\n"
        )
        self.bin = self.dir / "m"
        subprocess.run(["cc", "-O1", "-Wl,--build-id=sha1", "-o", str(self.bin), str(src)], check=True)
        self.stripped = self.dir / "m.stripped"
        subprocess.run(["strip", "-o", str(self.stripped), str(self.bin)], check=True)

    def tearDown(self) -> None:
        shutil.rmtree(self.dir)

    def order(self, *profiles: Path, **env: str) -> subprocess.CompletedProcess[str]:
        return run([str(BOLT), "order", str(self.bin), str(self.dir / "out.order"), *map(str, profiles)], env={"TMPDIR": str(self.dir), **env})

    def stand_in_perf(self, branches: bool) -> Path:
        """A perf whose recordings ran /lab/bin/kiln (this test's binary, by its build id) and a library."""
        calls = self.dir / "perf.calls"
        return script(
            self.dir / "perf",
            f"""printf '%s\\n' "$*" >> {calls}
case $1 in
  buildid-list) echo "1111111111111111111111111111111111111111 /usr/lib/libc.so.6"; echo "{build_id(self.bin)} /lab/bin/kiln" ;;
  evlist) echo "cycles:u: sample_type: IP|TID|TIME{'|BRANCH_STACK' if branches else ''}" ;;
  report)
    symfs=$(printf '%s\\n' "$@" | sed -n 's/^--symfs=//p')
    [ "$(readlink "$symfs/lab/bin/kiln")" = "{self.bin}" ] || {{ echo "no symbols for /lab/bin/kiln under $symfs" >&2; exit 1; }}
    case " $* " in
      *" --branch-stack "*) printf '# Samples: 1K\\n#\\n        5000  [.] warm  0.00  [  0.0%%]\\n          40  [.] rarely  0.00  [  0.0%%]\\n' ;;
      *) printf '# Samples: 3K\\n#\\n         900  [.] hot\\n          20  [.] warm\\n           5  [k] 0xffffffff8b401630\\n           3  [.] 0x0000000000401234\\n          10  [.] hot\\n' ;;
    esac ;;
esac
""",
        )

    def test_g11_s5_an_order_is_what_perf_saw_run_hottest_first(self) -> None:
        data = self.dir / "perf.data"
        data.write_bytes(b"recorded")
        r = self.order(data, PERF=str(self.stand_in_perf(branches=True)))
        self.assertEqual(r.returncode, 0, r.stderr)
        # warm: 20 samples and 5000 branch records; hot: 900 and 10, one name; the kernel's and an unnamed address out.
        self.assertEqual((self.dir / "out.order").read_text().splitlines(), ["warm", "hot", "rarely"])
        reports = [l for l in (self.dir / "perf.calls").read_text().splitlines() if l.startswith("report")]
        self.assertEqual(len(reports), 2, "the samples, then the branch records")
        for l in reports:
            self.assertIn("--dsos /lab/bin/kiln", l, "the binary's functions alone, not its libraries'")
            self.assertIn("--no-demangle", l, "names as the linker knows them")
        self.assertEqual(list(self.dir.glob("bolt-order.*")), [], "nothing left behind")

    def test_g11_s5_a_recording_without_branch_records_orders_by_samples(self) -> None:
        data = self.dir / "perf.data"
        data.write_bytes(b"recorded")
        r = self.order(data, PERF=str(self.stand_in_perf(branches=False)))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.dir / "out.order").read_text().splitlines(), ["hot", "warm"])

    def test_g11_s5_bolts_profiles_order_by_their_counts(self) -> None:
        lbr, samples = self.dir / "a.fdata", self.dir / "b.fdata"
        # A branch's ends, a local function's /<n> dropped; an end in no function (0) passed over.
        lbr.write_text("1 hot/1 10 1 cold/2 0 0 7\n0 [unknown] 7f00 1 entry 0 0 3\n1 entry 4 0 [unknown] 7f00 0 2\n")
        samples.write_text("no_lbr cycles:u:\n1 hot/1 3 100\n")
        r = self.order(lbr, samples)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.dir / "out.order").read_text().splitlines(), ["hot", "cold", "entry"])
        self.assertIn("3 functions, hottest first, from 2 profile(s)", r.stdout)

    def test_g11_s5_an_order_takes_the_symbols_of_the_build_that_ran(self) -> None:
        data = self.dir / "perf.data"
        data.write_bytes(b"recorded")
        r = run([str(BOLT), "order", str(self.stripped), str(self.dir / "out.order"), str(data)], env={"PERF": str(self.stand_in_perf(True))})
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("no symbols", r.stderr)
        # A recording of another build: no binary with this one's build id.
        other = self.dir / "other.c"
        other.write_text("int main(void) { return 0; }\n")
        subprocess.run(["cc", "-Wl,--build-id=sha1", "-o", str(self.bin), str(other)], check=True)
        r = self.order(data, PERF=str(self.dir / "perf"))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("recorded no binary with", r.stderr)
        self.assertFalse((self.dir / "out.order").exists())

    def test_g11_s5_an_order_from_a_real_perf_names_a_stripped_runs_functions(self) -> None:
        data = self.dir / "perf.data"
        rec = run(["perf", "record", "-q", "-e", "cycles:u", "-o", str(data), "--", str(self.stripped)]) if shutil.which("perf") else None
        if rec is None or rec.returncode not in (0, 1) or not data.exists() or data.stat().st_size == 0:
            self.skipTest("no perf that may record here")
        r = self.order(data)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.dir / "out.order").read_text().splitlines()[0], "hot", "named from the unstripped build's symbols")


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

    def test_g11_s5_an_order_is_taken_as_a_profile_is(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            Path(d, "kiln.order").write_text("hot\n")
            r = run([str(BUILD), "aarch64-unknown-linux-musl"], env={"KILN_RELEASE_BOLT": "kiln.order"}, cwd=Path(d))
            self.assertEqual(r.returncode, 1)
            self.assertNotIn("is no profile", r.stderr)
            self.assertIn("lays out x86_64-unknown-linux-musl", r.stderr)
            Path(d, "empty.order").write_text("")
            r = run([str(BUILD), "x86_64-unknown-linux-musl"], env={"KILN_RELEASE_BOLT": "empty.order"}, cwd=Path(d))
            self.assertEqual(r.returncode, 1)
            self.assertIn("relocs, a profile's path or an order's", r.stderr)


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

    def test_g11_s5_the_window_end_says_the_binarys_pages_by_section(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            p = subprocess.Popen(["sleep", "30"])
            try:
                time.sleep(0.2)
                r = run([str(HOOK), "window-end", str(p.pid), str(Path(d, "r.hook"))], env={"BOLT_RECORD": "none"})
                self.assertEqual(r.returncode, 0, r.stderr)
                pages = Path(d, "r.hook", "pages.txt").read_text().splitlines()
                rows = Path(d, "r.hook", "sections.txt").read_text().splitlines()
                kinds = Path(d, "r.hook", "kinds.txt").read_text().splitlines()
            finally:
                p.kill()
                p.wait()
            # The sections' resident pages are the mappings' (smaps says the same pages).
            self.assertEqual(rows[-1].split()[2], pages[-2].replace("binary rss_kb=", "resident_kb="), (rows, pages))
            # And the whole process's by kind (goal 11 step 5, part 6), the binary's among them.
            self.assertEqual(kinds[0].split()[0], "binary-code", kinds)
            self.assertTrue(kinds[-1].startswith("total rss_kb="), kinds)


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


SECTIONS = ROOT / "scripts" / "memory_lab" / "sections.py"


@unittest.skipUnless(sys.platform.startswith("linux"), "sections.py reads Linux's /proc")
class Sections(unittest.TestCase):
    """sections.py: a process's resident pages of its binary, by section, from pagemap."""

    def fields(self, line: str) -> tuple[str, dict[str, int]]:
        name, *kv = line.split()
        return name, {k: int(v) for k, v in (f.split("=") for f in kv)}

    def test_g11_s5_every_resident_page_of_the_binary_is_counted_once_by_its_sections(self) -> None:
        p = subprocess.Popen(["sleep", "30"])
        try:
            time.sleep(0.2)
            r = run([sys.executable, "-I", str(SECTIONS), str(p.pid)])
            exe = os.readlink(f"/proc/{p.pid}/exe")
            smaps = Path(f"/proc/{p.pid}/smaps").read_text().splitlines()
        finally:
            p.kill()
            p.wait()
        self.assertEqual(r.returncode, 0, r.stderr)
        rows = [self.fields(l) for l in r.stdout.splitlines()]
        self.assertEqual(rows[-1][0], "total")
        self.assertEqual(rows[-2][0], "(headers-and-gaps)")
        sections = dict(rows[:-1])
        self.assertGreater(sections[".text"]["resident_kb"], 0, "sleep ran its code")
        self.assertEqual(sections[".text"]["anon_kb"], 0, "its code is the file's own")
        total = rows[-1][1]
        # What smaps says the executable's mappings hold.
        rss, mine = 0, False
        for l in smaps:
            if l and l[0] in "0123456789abcdef" and "-" in l.split()[0]:
                mine = l.split(maxsplit=5)[5:] == [exe]
            elif mine and l.startswith("Rss:"):
                rss += int(l.split()[1])
        self.assertEqual(total["resident_kb"], rss)
        self.assertEqual(total["resident_kb"], total["file_kb"] + total["anon_kb"])
        # Each row's bytes rounded up to a KiB, so the rows' sum is the total's, give or take a KiB a row.
        summed = sum(f["resident_kb"] for _, f in rows[:-1])
        self.assertTrue(total["resident_kb"] <= summed <= total["resident_kb"] + len(rows), (summed, total))
        for name, f in rows[:-2]:
            self.assertLessEqual(f["windows"], f["of"], name)

    def test_g11_s5_a_process_that_isnt_there_is_said(self) -> None:
        r = run([sys.executable, "-I", str(SECTIONS), "999999999"])
        self.assertEqual(r.returncode, 1)
        self.assertIn("process 999999999", r.stderr)
        r = run([sys.executable, "-I", str(SECTIONS), "self"])
        self.assertEqual(r.returncode, 2)


KINDS = ROOT / "scripts" / "memory_lab" / "kinds.py"

# A process holding what kinds.py tells apart: 6 threads waiting (their stacks), 4 MiB advised MADV_HUGEPAGE and
# 4 MiB advised MADV_NOHUGEPAGE, each written whole, then it says it's ready and waits for its stdin to close.
HOLDER = """
import mmap, sys, threading
done = threading.Event()
threads = [threading.Thread(target=done.wait) for _ in range(6)]
for t in threads:
    t.start()
held = []
for advice in (mmap.MADV_HUGEPAGE, mmap.MADV_NOHUGEPAGE):
    m = mmap.mmap(-1, 4 << 20, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
    try:
        m.madvise(advice)
    except OSError:
        pass
    m.write(b"x" * (4 << 20))
    held.append(m)
print("ready", flush=True)
sys.stdin.read()
done.set()
"""


@unittest.skipUnless(sys.platform.startswith("linux"), "kinds.py reads Linux's /proc")
class Kinds(unittest.TestCase):
    """kinds.py: a process's resident memory by kind, from smaps (goal 11, plan 01a1a87b2e56 step 5)."""

    def fields(self, line: str) -> tuple[str, dict[str, int]]:
        name, *kv = line.split()
        return name, {k: int(v) for k, v in (f.split("=") for f in kv)}

    def test_g11_s5_each_kind_is_told_apart_and_the_kinds_sum_to_the_processs_rss(self) -> None:
        p = subprocess.Popen([sys.executable, "-I", "-c", HOLDER], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        try:
            self.assertEqual(p.stdout.readline().strip() if p.stdout else "", "ready")
            r = run([sys.executable, "-I", str(KINDS), str(p.pid)])
            rollup = Path(f"/proc/{p.pid}/smaps_rollup").read_text()
            flags = Path("/sys/kernel/mm/transparent_hugepage/enabled").read_text() if Path("/sys/kernel/mm/transparent_hugepage/enabled").exists() else ""
        finally:
            if p.stdin:
                p.stdin.close()
            p.wait(timeout=30)
            if p.stdout:
                p.stdout.close()
        self.assertEqual(r.returncode, 0, r.stderr)
        rows = dict(self.fields(l) for l in r.stdout.splitlines())
        self.assertEqual(list(rows), ["binary-code", "binary-ro", "binary-rw", "files", "stacks", "thp-advised", "allocator", "heap", "other-anon", "kernel", "total"])
        rss = next(int(l.split()[1]) for l in rollup.splitlines() if l.startswith("Rss:"))
        self.assertEqual(rows["total"]["rss_kb"], rss, "every mapping counted once: the process's Rss")
        self.assertEqual(sum(f["rss_kb"] for k, f in rows.items() if k != "total"), rss)
        # The main thread's stack and the six waiting threads' (glibc's, one mapping for adjacent ones where the
        # kernel puts their guards in them).
        self.assertGreaterEqual(rows["stacks"]["maps"], 2, rows["stacks"])
        self.assertGreater(rows["stacks"]["rss_kb"], 0)
        if "[never]" not in flags and flags:
            # Each 4 MiB written whole: the advised mappings hold it all (no other mapping of this process is advised).
            self.assertGreaterEqual(rows["thp-advised"]["rss_kb"], 4096, rows["thp-advised"])
            self.assertGreaterEqual(rows["allocator"]["rss_kb"], 4096, rows["allocator"])
            self.assertLess(rows["thp-advised"]["rss_kb"], 8192, rows["thp-advised"])
        self.assertGreater(rows["binary-code"]["rss_kb"], 0, "python's own code")

    def test_g11_s5_musls_stacks_jemallocs_turso_and_the_c_heap_each_take_their_kind(self) -> None:
        # The production build's layout (static musl, jemalloc), as its smaps lists it, lowest first.
        import importlib.util

        spec = importlib.util.spec_from_file_location("kinds", KINDS)
        assert spec and spec.loader
        kinds = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(kinds)
        KiB, MiB = 1 << 10, 1 << 20
        at = [0x7F0000000000]

        def m(size: int, perms: str = "rw-p", path: str = "", flags: str = "rd wr mr mw me ac sd", rss: int = 4) -> dict:
            lo = at[0]
            at[0] += size
            return dict(lo=lo, hi=lo + size, perms=perms, path=path, rss=rss, anon=rss, huge=0, flags=set(flags.split()))

        maps = [
            m(4 * KiB, "---p", flags="mr mw me sd", rss=0),  # a runtime thread: musl's guard,
            m(16 * MiB, flags="rd wr mr mw me ac sd nh", rss=88),  # its stack, advised MADV_NOHUGEPAGE by kiln,
            m(12 * KiB, rss=12),  # its control block and TLS above it
            m(4 * MiB, flags="rd wr mr mw me ac sd nh", rss=4096),  # jemalloc's extents
            m(4 * KiB, "---p", flags="mr mw me sd", rss=0),  # a signal stack's guard,
            m(12 * KiB, rss=0),  # the signal stack: not a thread's
            m(6164 * KiB, flags="rd wr mr mw me ac sd hg", rss=5760),  # turso's arenas
            m(4 * KiB, "---p", flags="mr mw me sd", rss=0),  # a thread the runtime didn't start: its guard,
            m(16 * MiB, rss=20),  # its stack (no advice),
            m(256 * KiB, rss=200),  # a C library's malloc group
            m(132 * KiB, rss=4),  # glibc's arena,
            m(65404 * KiB, "---p", flags="mr mw me sd", rss=0),  # its reserve,
            m(132 * KiB, rss=4),  # and the next arena, just above that reserve: not a stack
            m(180 * MiB, "r-xp", "/x/kiln", "rd ex mr mw me sd", rss=91000),
            m(30 * MiB, "r--p", "/x/kiln", "rd mr mw me sd", rss=6300),
            m(3 * MiB, "rw-p", "/x/kiln", "rd wr mr mw me ac sd", rss=3000),
            m(132 * KiB, path="[stack]", rss=200),
            m(8 * KiB, "r-xp", "[vdso]", "rd ex mr mw me de sd", rss=8),
        ]
        got = kinds.classify(maps, "/x/kiln")
        self.assertEqual(
            got,
            ["other-anon", "stacks", "stacks", "allocator", "other-anon", "other-anon", "thp-advised", "other-anon", "stacks", "other-anon", "other-anon", "other-anon", "other-anon", "binary-code", "binary-ro", "binary-rw", "stacks", "kernel"],
        )
        rows = kinds.tally(maps, got)
        self.assertEqual(rows["stacks"]["rss"], 88 + 12 + 20 + 200)
        self.assertEqual(rows["allocator"]["rss"], 4096)
        self.assertEqual(rows["thp-advised"]["rss"], 5760)

    def test_g11_s5_a_process_that_isnt_there_is_said(self) -> None:
        r = run([sys.executable, "-I", str(KINDS), "999999999"])
        self.assertEqual(r.returncode, 1)
        self.assertIn("process 999999999", r.stderr)
        r = run([sys.executable, "-I", str(KINDS), "self"])
        self.assertEqual(r.returncode, 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
