#!/usr/bin/env python3
"""Tests of the release's code layout scripts (goal 11, plan 01a1a87b2e56 step 5): ci/release/bolt.sh,
ci/release/zig-relocs.sh, ci/release/build.sh's KILN_RELEASE_BOLT, scripts/memory_lab/bolt-hook.sh, and the pipeline
and its A/B (scripts/memory_lab/bolt-lab.sh, bolt-read.py), each
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
        for s in (BOLT, RELOCS, BUILD, HOOK, ROOT / "scripts" / "memory_lab" / "bolt-lab.sh"):
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


@unittest.skipUnless(
    shutil.which("cc") and shutil.which("readelf") and shutil.which("strip") and os.uname().machine == "x86_64" and sys.platform.startswith("linux"),
    "takes cc, readelf and strip on x86-64 Linux",
)
class BuildBolt(unittest.TestCase):
    """build.sh's KILN_RELEASE_BOLT end to end, its tools stand-ins (zig's venv, cargo-zigbuild, rustup, llvm-bolt) in a
    copy of ci/release: the relocations build in a cargo-zigbuild cache of its own, and a layout shipped only as the same
    program."""

    ID = "00112233445566778899aabbccddeeff00112233"

    def setUp(self) -> None:
        self.dir = Path(tempfile.mkdtemp())
        root = self.dir / "tree"
        (root / "ci" / "release").mkdir(parents=True)
        for f in (BUILD, BOLT, RELOCS):
            shutil.copy2(f, root / "ci" / "release" / f.name)
        (root / "ui" / "dist" / "client").mkdir(parents=True)
        (root / "ui" / "dist" / "client" / "_shell.html").write_text("<html></html>\n")
        self.build = root / "ci" / "release" / "build.sh"
        self.src = self.dir / "kiln.c"
        self.src.write_text(
            "#include <stdio.h>\n#include <string.h>\n#ifndef VERSION\n#define VERSION \"kiln 1.2.3\"\n#endif\n"
            'int main(int c, char **v) { if (c > 1 && !strcmp(v[1], "--version")) { puts(VERSION); return 0; } return 1; }\n'
        )
        self.env_seen = self.dir / "zigbuild.env"
        zig = self.dir / "zig-home"
        (zig / "bin").mkdir(parents=True)
        script(zig / "bin" / "python", '[ "$*" = "-m ziglang version" ] && echo 0.17.0\n')
        # cargo-zigbuild's zigbuild: notes what it was given, and builds "kiln" as zig-relocs.sh would link it.
        script(
            zig / "bin" / "cargo-zigbuild",
            f"""[ "$1" = --help ] && exit 0
env | grep -E '^(CARGO_ZIGBUILD_[A-Z_]+|CARGO_TARGET_DIR|CARGO_PROFILE_RELEASE_STRIP|KILN_LINK_ORDER)=' | sort > {self.env_seen}
out=$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release
mkdir -p "$out/deps"
cc -O1 -no-pie -Wl,--emit-relocs -Wl,--build-id=0x{self.ID} -o "$out/deps/kiln-0123456789abcdef" {self.src} && cp "$out/deps/kiln-0123456789abcdef" "$out/kiln"
""",
        )
        tools = self.dir / "path"
        tools.mkdir()
        script(tools / "rustup", "exit 0\n")
        bolt = self.dir / "bolt-tools"
        bolt.mkdir()
        # llvm-bolt: the same build laid out elsewhere (its text moved, its build id's last bit flipped as llvm-bolt patches
        # it), saying LAID_OUT_VERSION.
        script(
            bolt / "llvm-bolt",
            f"""case $1 in --version) echo "  LLVM version 23.1.4"; exit 0 ;; esac
out=; prev=; for a in "$@"; do [ "$prev" = -o ] && out=$a; prev=$a; done
cc -O1 -no-pie -Wl,--emit-relocs -Wl,--build-id=0x{self.ID[:-2]}32 -Wl,-Ttext-segment=0x800000 "-DVERSION=\\"${{LAID_OUT_VERSION:-kiln 1.2.3}}\\"" -o "$out" {self.src}
echo "           123456789 : executed instructions"
""",
        )
        script(bolt / "perf2bolt", "exit 0\n")
        script(bolt / "merge-fdata", 'cat "$@"\n')
        probe = subprocess.run(["cc", "-no-pie", "-Wl,-Ttext-segment=0x800000", f"-Wl,--build-id=0x{self.ID}", "-o", str(self.dir / "probe"), str(self.src)], capture_output=True)
        if probe.returncode != 0:
            self.skipTest("this cc's linker takes no -Ttext-segment or explicit build id")
        self.env = {
            "KILN_ZIG_HOME": str(zig),
            "KILN_UI_SERVER": "off",
            "KILN_DIST": str(self.dir / "dist"),
            "CARGO_TARGET_DIR": str(self.dir / "target"),
            "KILN_BOLT_BIN": str(bolt),
            "PATH": f"{tools}:{os.environ['PATH']}",
            "TMPDIR": str(self.dir),
        }

    def tearDown(self) -> None:
        shutil.rmtree(self.dir)

    def test_g11_s5_the_relocations_build_keeps_a_zigbuild_cache_of_its_own(self) -> None:
        r = run([str(self.build), "x86_64-unknown-linux-musl"], env={**self.env, "KILN_RELEASE_BOLT": "relocs"})
        self.assertEqual(r.returncode, 0, r.stderr)
        seen = dict(l.split("=", 1) for l in self.env_seen.read_text().splitlines())
        bolt_dir = self.dir / "target" / "bolt"
        # Its own cache, so no other build of the target on the machine rewrites the wrapper that runs zig-relocs.sh.
        self.assertEqual(seen.get("CARGO_ZIGBUILD_CACHE_DIR"), str(bolt_dir / "zigbuild"))
        self.assertTrue(seen["CARGO_ZIGBUILD_ZIG_COMMAND"].endswith("ci/release/zig-relocs.sh"))
        self.assertEqual((seen["CARGO_TARGET_DIR"], seen["CARGO_PROFILE_RELEASE_STRIP"]), (str(bolt_dir), "false"))
        shipped = self.dir / "dist" / "kiln-x86_64-unknown-linux-musl"
        self.assertNotIn(".symtab", run(["readelf", "-SW", str(shipped)]).stdout, "the release as it ships: stripped")
        self.assertEqual(build_id(shipped), self.ID, "its build id the unstripped one's, as perf2bolt matches them")
        self.assertEqual((bolt_dir / "x86_64-unknown-linux-musl" / "release" / "kiln.linked-order").read_text().strip(), "none")

    def test_g11_s5_a_layout_ships_only_as_the_same_program(self) -> None:
        prof = self.dir / "kiln.fdata"
        prof.write_text("1 main 0 1 main 4 0 7\n")
        shipped = self.dir / "dist" / "kiln-x86_64-unknown-linux-musl"
        r = run([str(self.build), "x86_64-unknown-linux-musl"], env={**self.env, "KILN_RELEASE_BOLT": str(prof)})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("laid out (build id", r.stdout + r.stderr, "bolt.sh same said so")
        self.assertEqual(run([str(shipped), "--version"]).stdout.strip(), "kiln 1.2.3")
        self.assertTrue((self.dir / "target" / "bolt" / "x86_64-unknown-linux-musl" / "release" / "kiln.bolt.txt").exists())
        # A layout that isn't the same program fails the build, and doesn't stay in dist/.
        r = run([str(self.build), "x86_64-unknown-linux-musl"], env={**self.env, "KILN_RELEASE_BOLT": str(prof), "LAID_OUT_VERSION": "kiln 9.9.9"})
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("says 'kiln 9.9.9'", r.stderr)
        self.assertFalse(shipped.exists())


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
            # Neither of a profiling round's gdb calls without its switch.
            self.assertFalse(Path(d, "r.hook", "heap-dump.log").exists())
            self.assertFalse(Path(d, "r.hook", "purge.txt").exists())

    def test_g11_s5_a_profiling_round_dumps_the_heap_and_probes_a_purge_at_the_window_end(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            calls = Path(d, "gdb.calls")
            # A gdb that says what it was asked, as gdb -batch would run it.
            gdb = script(Path(d, "gdb"), f'printf "%s\\n" "$*" >> {calls}\n')
            p = subprocess.Popen(["sleep", "30"])
            try:
                time.sleep(0.2)
                r = run([str(HOOK), "window-end", str(p.pid), str(Path(d, "r.hook"))], env={"BOLT_RECORD": "none", "HEAP_DUMP": "1", "PURGE_PROBE": "1", "GDB": str(gdb)})
            finally:
                p.kill()
                p.wait()
            self.assertEqual(r.returncode, 0, r.stderr)
            asked = calls.read_text().splitlines()
            self.assertEqual(len(asked), 2, asked)
            self.assertIn(f"-p {p.pid} -batch -nx -q -ex call (int)_rjem_mallctl(\"prof.dump\"", asked[0])
            self.assertIn("_rjem_mallctl(\"arena.4096.purge\"", asked[1])
            purge = Path(d, "r.hook", "purge.txt").read_text().splitlines()
            self.assertEqual([l.split(":")[0] for l in purge], ["before", "Rss", "Anonymous", "after", "Rss", "Anonymous"], purge)
            self.assertEqual(Path(d, "r.hook", "hook.log").read_text() if Path(d, "r.hook", "hook.log").exists() else "", "")


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
            m(4 * KiB, "---p", flags="mr mw me sd nh", rss=0),  # one mapped MAP_STACK (`nh`) on a newer kernel,
            m(8 * KiB, flags="rd wr mr mw me ac sd nh", rss=4),  # and its stack: not the allocator's either
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
            ["other-anon", "stacks", "stacks", "allocator", "other-anon", "other-anon", "other-anon", "other-anon", "thp-advised", "other-anon", "stacks", "other-anon", "other-anon", "other-anon", "other-anon", "binary-code", "binary-ro", "binary-rw", "stacks", "kernel"],
        )
        rows = kinds.tally(maps, got)
        self.assertEqual(rows["stacks"]["rss"], 88 + 12 + 20 + 200)
        self.assertEqual(rows["allocator"]["rss"], 4096)
        self.assertEqual(rows["thp-advised"]["rss"], 5760)

    def test_g11_s5_a_saved_smaps_is_read_as_the_live_process_is(self) -> None:
        # The lab keeps a round's smaps at the window's end (smaps.window-end): kinds.py reads a copy as it reads the
        # process, the executable the first file it maps unless said.
        p = subprocess.Popen([sys.executable, "-I", "-c", HOLDER], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        with tempfile.TemporaryDirectory() as d:
            try:
                self.assertEqual(p.stdout.readline().strip() if p.stdout else "", "ready")
                copy = Path(d, "smaps.window-end")
                copy.write_text(Path(f"/proc/{p.pid}/smaps").read_text())
                exe = os.readlink(f"/proc/{p.pid}/exe")
                live = run([sys.executable, "-I", str(KINDS), str(p.pid)])
            finally:
                if p.stdin:
                    p.stdin.close()
                p.wait(timeout=30)
                if p.stdout:
                    p.stdout.close()
            said = run([sys.executable, "-I", str(KINDS), str(copy), exe])
            guessed = run([sys.executable, "-I", str(KINDS), str(copy)])
        self.assertEqual(said.returncode, 0, said.stderr)
        # The same mappings, but for what changed between the two reads (a page or two of the live process's heap).
        live_rows, said_rows = dict(self.fields(l) for l in live.stdout.splitlines()), dict(self.fields(l) for l in said.stdout.splitlines())
        for kind in ("binary-code", "stacks", "thp-advised", "allocator"):
            self.assertLessEqual(abs(live_rows[kind]["rss_kb"] - said_rows[kind]["rss_kb"]), 64, kind)
        self.assertEqual(guessed.stdout, said.stdout, "the first file mapped is the executable")
        bad = run([sys.executable, "-I", str(KINDS), str(Path(d, "gone"))])
        self.assertEqual(bad.returncode, 1)

    def test_g11_s5_a_process_that_isnt_there_is_said(self) -> None:
        r = run([sys.executable, "-I", str(KINDS), "999999999"])
        self.assertEqual(r.returncode, 1)
        self.assertIn("process 999999999", r.stderr)
        r = run([sys.executable, "-I", str(KINDS)])
        self.assertEqual(r.returncode, 2)
        r = run([sys.executable, "-I", str(KINDS), "1", "2", "3"])
        self.assertEqual(r.returncode, 2)


LAB = ROOT / "scripts" / "memory_lab" / "bolt-lab.sh"
READ = ROOT / "scripts" / "memory_lab" / "bolt-read.py"


class Record(unittest.TestCase):
    """bolt.sh record and lbr (goal 11 step 5, the pipeline whole): LBR where this machine's perf records branch stacks,
    else cycles:u samples with their call graphs, and which it took said."""

    def setUp(self) -> None:
        self.dir = Path(tempfile.mkdtemp())
        self.calls = self.dir / "perf.calls"
        # A perf with branch stacks when LBR=1: a recording's -j opens then, and its sample type says BRANCH_STACK.
        self.perf = script(
            self.dir / "perf",
            f"""printf '%s\\n' "$*" >> {self.calls}
arg() {{ local want=$1 prev=; shift; for a in "$@"; do [ "$prev" = "$want" ] && echo "$a"; prev=$a; done; }}
case $1 in
  record)
    case " $* " in *" -j "*) [ "${{LBR:-0}}" = 1 ] || {{ echo "PMU Hardware or event type doesn't support branch stack sampling." >&2; exit 255; }} ;; esac
    case " $* " in *" -j "*) echo branches > "$(arg -o "$@")" ;; *) echo samples > "$(arg -o "$@")" ;; esac ;;
  evlist) if grep -q branches "$(arg -i "$@")"; then echo "cycles:u: sample_type: IP|TID|BRANCH_STACK"; else echo "cycles:u: sample_type: IP|TID"; fi ;;
esac
""",
        )

    def tearDown(self) -> None:
        shutil.rmtree(self.dir)

    def record(self, kind: str | None, lbr: str) -> subprocess.CompletedProcess[str]:
        args = [str(BOLT), "record", "4242", str(self.dir / "perf.data")] + ([kind] if kind else [])
        return run(args, env={"PERF": str(self.perf), "LBR": lbr, "TMPDIR": str(self.dir), "BOLT_EVENT": ""})

    def recorded(self) -> str:
        return next(l for l in self.calls.read_text().splitlines() if " -p 4242 " in f" {l} ")

    def test_g11_s5_auto_records_branch_stacks_where_the_cpu_has_them(self) -> None:
        r = self.record(None, "1")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("-e cycles:u -j any,u -p 4242", self.recorded(), "BOLT's README's recording")
        self.assertIn("with its branch stacks (LBR", r.stderr)
        self.assertEqual(list(self.dir.glob("bolt-lbr.*")), [], "the probe's recording isn't left behind")

    def test_g11_s5_auto_records_samples_with_call_graphs_where_it_has_none(self) -> None:
        r = self.record("auto", "0")
        self.assertEqual(r.returncode, 0, r.stderr)
        rec = self.recorded()
        self.assertNotIn(" -j ", f" {rec} ")
        self.assertIn("-e cycles:u --call-graph dwarf -p 4242", rec)
        self.assertIn("samples with their call graphs, this machine's perf recording no branch stacks", r.stderr)

    def test_g11_s5_a_kind_asked_for_is_taken_without_asking_perf(self) -> None:
        r = self.record("samples", "1")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.calls.read_text().splitlines()), 1, "no probe")
        self.assertIn("--call-graph dwarf", self.recorded())
        r = self.record("bogus", "1")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("auto, lbr or samples", r.stderr)

    def test_g11_s5_lbr_says_whether_this_machine_records_branch_stacks(self) -> None:
        env = {"PERF": str(self.perf), "TMPDIR": str(self.dir), "BOLT_EVENT": ""}
        yes, no = run([str(BOLT), "lbr"], env={**env, "LBR": "1"}), run([str(BOLT), "lbr"], env={**env, "LBR": "0"})
        self.assertEqual((yes.returncode, no.returncode), (0, 1))
        self.assertIn("branch stacks: yes", yes.stdout)
        self.assertIn("branch stacks: no", no.stdout)


class ProfileSays(unittest.TestCase):
    """bolt.sh profile: each recording said to hold branch records, or samples alone, which perf2bolt takes with -ba."""

    def test_g11_s5_each_recording_is_said_lbr_or_samples(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            dd, tools = Path(d), Path(d, "tools")
            tools.mkdir()
            calls = dd / "perf2bolt.calls"
            script(tools / "llvm-bolt", "echo '  LLVM version 23.1.4'\n")
            script(tools / "perf2bolt", f'printf "%s\\n" "$*" >> {calls}\nwhile [ $# -gt 0 ]; do if [ "$1" = -o ]; then shift; echo "1 f 0 1 f 4 0 7" > "$1"; fi; shift; done\n')
            script(tools / "merge-fdata", 'cat "$@"\n')
            perf = script(dd / "perf", 'case $1 in evlist) i=$(printf "%s\\n" "$@" | sed -n "/^-i$/{n;p}"); if grep -q lbr "$i"; then echo "sample_type: IP|BRANCH_STACK"; else echo "sample_type: IP"; fi ;; esac\n')
            a, b = dd / "a.data", dd / "b.data"
            a.write_text("lbr")
            b.write_text("plain")
            r = run([str(BOLT), "profile", "/bin/true", str(dd / "out.fdata"), str(a), str(b)], env={"KILN_BOLT_BIN": str(tools), "PERF": str(perf)})
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn(f"{a}: branch records (LBR)", r.stderr)
            self.assertIn(f"{b}: samples alone, no branch records: perf2bolt's basic aggregation (--ba)", r.stderr)
            first, second = calls.read_text().splitlines()
            self.assertNotIn("--ba", first)
            self.assertIn("--ba", second)
            self.assertNotIn(" -w ", f" {first} ", "no YAML unless asked")

    def test_g11_s5_the_yaml_comes_from_the_same_pass(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            dd, tools = Path(d), Path(d, "tools")
            tools.mkdir()
            calls = dd / "perf2bolt.calls"
            script(tools / "llvm-bolt", "echo '  LLVM version 23.1.4'\n")
            script(
                tools / "perf2bolt",
                f'printf "%s\\n" "$*" >> {calls}\nwhile [ $# -gt 0 ]; do case $1 in -o) shift; echo "1 f 0 1 f 4 0 7" > "$1" ;; -w) shift; echo "--- yaml" > "$1" ;; esac; shift; done\n',
            )
            script(tools / "merge-fdata", 'cat "$@"\n')
            perf = script(dd / "perf", 'case $1 in evlist) echo "sample_type: IP|BRANCH_STACK" ;; esac\n')
            a, b = dd / "a.data", dd / "b.data"
            a.write_text("lbr")
            b.write_text("lbr")
            env = {"KILN_BOLT_BIN": str(tools), "PERF": str(perf), "BOLT_PROFILE_YAML": str(dd / "out.yaml")}
            r = run([str(BOLT), "profile", "/bin/true", str(dd / "out.fdata"), str(a), str(b)], env=env)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(len(calls.read_text().splitlines()), 2, "one perf2bolt a recording, not two")
            self.assertEqual((dd / "out.fdata").read_text(), "1 f 0 1 f 4 0 7\n" * 2)
            self.assertEqual((dd / "out.yaml").read_text(), "--- yaml\n" * 2, "the recordings' YAML merged too")
            self.assertEqual(sorted(p.name for p in dd.glob("out.*") if not p.name.endswith(".log")), ["out.fdata", "out.yaml"], "no parts left")


def bolt_tools(d: Path, log: str, calls: Path) -> Path:
    """Stand-ins for BOLT's three tools: an llvm-bolt that copies its input to its output and says `log`."""
    tools = d / "tools"
    tools.mkdir()
    script(
        tools / "llvm-bolt",
        f"""case $1 in --version) echo "  LLVM version 23.1.4"; exit 0 ;; esac
printf '%s\\n' "$*" >> {calls}
out=; prev=; for a in "$@"; do [ "$prev" = -o ] && out=$a; prev=$a; done
cp "$1" "$out"
# llvm-bolt's note: its command line, this machine's paths in it.
printf 'BOLT revision: <unknown>, command line: llvm-bolt %s' "$*" > "$out.note"
objcopy --add-section .note.bolt_info="$out.note" "$out" && rm -f "$out.note"
cat <<'LOG'
{log}LOG
""",
    )
    script(tools / "perf2bolt", "exit 0\n")
    script(tools / "merge-fdata", 'cat "$@"\n')
    return tools


@unittest.skipUnless(
    shutil.which("cc") and shutil.which("readelf") and shutil.which("strip") and shutil.which("objcopy") and os.uname().machine == "x86_64",
    "takes cc, readelf, strip and objcopy on x86-64",
)
class Covered(unittest.TestCase):
    """bolt.sh optimize: BOLT's README's options and safe ICF, and what the profile covered said beside the log."""

    LOG = (
        "BOLT-INFO: enabling lite mode\n"
        "BOLT-INFO: 4123 out of 180000 functions in the binary (2.3%) have non-empty execution profile\n"
        "BOLT-INFO: splitting separates 2400000 hot bytes from 9000000 cold bytes (21.05% of split functions is hot).\n"
        "BOLT-INFO: ICF folded 300 out of 180000 functions in 3 passes. 0 functions had jump tables.\n"
        "BOLT-WARNING: one warning\n"
        "BOLT-INFO: program-wide dynostats after all optimizations before SCTC and FOP (no change):\n\n"
        "           123456789 : executed instructions\n"
        "BOLT-INFO: program-wide dynostats after all optimizations:\n"
        "           120000000 : executed instructions (-2.8%)\n"
    )

    def optimize(self, dd: Path, log: str) -> tuple[subprocess.CompletedProcess[str], Path, Path]:
        (dd / "built").mkdir()
        (dd / "dist").mkdir()
        src, binary = dd / "m.c", dd / "built" / "kiln"
        src.write_text("int main(void) { return 0; }\n")
        subprocess.run(["cc", "-Wl,--emit-relocs", "-o", str(binary), str(src)], check=True)
        tools = bolt_tools(dd, log, dd / "bolt.calls")
        prof = dd / "kiln.fdata"
        prof.write_text("1 main 0 1 main 4 0 7\n")
        out = dd / "dist" / "kiln-x86_64-unknown-linux-musl"
        return run([str(BOLT), "optimize", str(binary), str(prof), str(out)], env={"KILN_BOLT_BIN": str(tools)}), binary, out

    def test_g11_s5_a_log_without_warnings_is_said_as_none(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            r, binary, out = self.optimize(Path(d), self.LOG.replace("BOLT-WARNING: one warning\n", ""))
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("warnings: 0", Path(f"{binary}.bolt.txt").read_text())
            self.assertTrue(out.exists())

    def test_g11_s5_the_profiles_coverage_is_said_beside_the_log_not_in_dist(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            dd = Path(d)
            r, binary, out = self.optimize(dd, self.LOG)
            calls = dd / "bolt.calls"
            self.assertEqual(r.returncode, 0, r.stderr)
            said = Path(f"{binary}.bolt.txt").read_text()
            self.assertIn("instructions: 123456789 ", said, "the first count, the profile's, before BOLT changed anything")
            for want in ("llvm-bolt: 23.1.4", "have non-empty execution profile", "enabling lite mode", "splitting separates", "ICF folded", "warnings: 1"):
                self.assertIn(want, said)
            self.assertEqual([p.name for p in (dd / "dist").iterdir()], [out.name], "dist/ holds the binary alone: build.sh's SHA256SUMS takes every kiln-*")
            opts = calls.read_text()
            for o in ("-reorder-blocks=ext-tsp", "-reorder-functions=cdsort", "-split-functions", "-split-all-cold", "-split-eh", "-dyno-stats", "-icf=safe"):
                self.assertIn(o, opts.split())
            self.assertNotIn("-use-gnu-stack", opts, "musl sizes its threads' stacks by PT_GNU_STACK")
            self.assertIn(".note.bolt_info", run(["readelf", "-SW", f"{binary}.bolt"]).stdout, "llvm-bolt's own output has its note")
            self.assertNotIn(".note.bolt_info", run(["readelf", "-SW", str(out)]).stdout, "the shipped layout doesn't: one build and one profile, one layout's bytes")


@unittest.skipUnless(
    shutil.which("cc") and shutil.which("readelf") and os.uname().machine == "x86_64" and sys.platform.startswith("linux"),
    "takes cc and readelf on x86-64 Linux",
)
class Same(unittest.TestCase):
    """bolt.sh same: one build id and one `--version`, its code moved: a layout of the build, not another build."""

    ID = "00112233445566778899aabbccddeeff00112233"

    def build(self, d: Path, name: str, version: str = "kiln 1.2.3", build_id: str = ID, moved: bool = False) -> Path:
        src = d / f"{name}.c"
        src.write_text(
            "#include <stdio.h>\n#include <string.h>\n"
            f'int main(int c, char **v) {{ if (c > 1 && !strcmp(v[1], "--version")) {{ puts("{version}"); return 0; }} return 1; }}\n'
        )
        out = d / name
        cmd = ["cc", "-O1", "-no-pie", f"-Wl,--build-id=0x{build_id}", "-o", str(out), str(src)] + (["-Wl,-Ttext-segment=0x800000"] if moved else [])
        if subprocess.run(cmd, capture_output=True).returncode != 0:
            self.skipTest("this cc's linker takes no -Ttext-segment or explicit build id")
        return out

    def test_g11_s5_a_layout_of_the_build_is_the_same_program(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            a, b = self.build(Path(d), "a"), self.build(Path(d), "b", moved=True)
            r = run([str(BOLT), "same", str(a), str(b)])
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("both 'kiln 1.2.3'", r.stdout)
            # llvm-bolt patches its output's build id, its last bit flipped ("BOLT-INFO: patched build-id (flipped
            # last bit)", 23.1.4 on kiln's release, 2026-11-06): still the build's.
            bolted = self.build(Path(d), "bolted", build_id=self.ID[:-2] + "32", moved=True)
            r = run([str(BOLT), "same", str(a), str(bolted)])
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("its last bit flipped as BOLT patches it", r.stdout)

    def test_g11_s5_the_build_itself_another_build_or_another_version_isnt(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            dd = Path(d)
            a = self.build(dd, "a")
            r = run([str(BOLT), "same", str(a), str(a)])
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("not laid out", r.stderr)
            other = self.build(dd, "other", build_id="ff" * 20, moved=True)
            r = run([str(BOLT), "same", str(a), str(other)])
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("isn't a layout of", r.stderr)
            says = self.build(dd, "says", version="kiln 9.9.9", moved=True)
            r = run([str(BOLT), "same", str(a), str(says)])
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("says 'kiln 9.9.9'", r.stderr)


# A stand-in kiln-lab: notes its arguments and its hook's environment, and writes a run as kiln-lab does (run.json, and each
# round's hook folder with what bolt-hook.sh would have left: a recording when its record asks for one, sections.txt).
STANDIN_LAB = """#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
d = Path(os.environ["STANDIN_DIR"])
with open(d / "calls", "a") as f:
    f.write(" ".join(args) + "\\n")
with open(d / "envs", "a") as f:
    f.write(" ".join(f"{k}={os.environ.get(k, '')}" for k in ("BOLT_RECORD", "KILN_LAB_HOOK", "BOLT_SH", "BOLT_FREQ")) + "\\n")
opt = lambda k, default=None: args[args.index(k) + 1] if k in args else default
out = Path(opt("--out"))
labels = [opt("--label", "build")] if args[0] == "run" else [opt("--label-a"), opt("--label-b")]
sections = {
    labels[0]: ".text size_kb=180000 resident_kb=96000 file_kb=96000 anon_kb=0 windows=1500 of=2800\\n.rodata size_kb=32000 resident_kb=6000 file_kb=6000 anon_kb=0 windows=200 of=500\\ntotal size_kb=230000 resident_kb=104000 file_kb=104000 anon_kb=0 windows=1750 of=3600\\n",
    labels[-1]: ".text size_kb=5000 resident_kb=4000 file_kb=4000 anon_kb=0 windows=70 of=80\\n.text.cold size_kb=9000 resident_kb=200 file_kb=200 anon_kb=0 windows=10 of=150\\n.bolt.org.text size_kb=180000 resident_kb=10000 file_kb=10000 anon_kb=0 windows=200 of=2800\\n.bolt.org.rodata size_kb=32000 resident_kb=3000 file_kb=3000 anon_kb=0 windows=90 of=500\\n.rodata size_kb=100 resident_kb=100 file_kb=100 anon_kb=0 windows=3 of=3\\n.rodata.cold size_kb=40 resident_kb=40 file_kb=40 anon_kb=0 windows=1 of=2\\ntotal size_kb=244000 resident_kb=22000 file_kb=22000 anon_kb=0 windows=500 of=3700\\n",
}
rounds, index = [], 0
for i in range(int(opt("--rounds", "8"))):
    for label in labels:
        index += 1
        hook = out / "rounds" / f"{index:03d}-{label}.hook"
        hook.mkdir(parents=True, exist_ok=True)
        if os.environ.get("BOLT_RECORD") in ("auto", "lbr", "samples"):
            (hook / "perf.data").write_text("recorded")
            (hook / "perf.log").write_text(f"bolt.sh: recording process {1000 + index} with its branch stacks (LBR: -e cycles:u -j any,u)\\n")
        (hook / "sections.txt").write_text(sections[label])
        errors = float(os.environ.get("STANDIN_ERRORS_B", "0")) if label == labels[-1] and len(labels) > 1 else 0.0
        cpu = 7.0 + i if label == labels[-1] and len(labels) > 1 else 9.0 + i
        rounds.append({"build": label, "index": index, "numbers": {"server.cpu_ms_per_request": cpu, "server.rss_file_mb": 100.0, "api_read.p50_ms": 0.2, "api_read.errors": errors}})
out.mkdir(parents=True, exist_ok=True)
failures = [os.environ["STANDIN_FAIL"]] if os.environ.get("STANDIN_FAIL") else []
(out / "run.json").write_text(json.dumps({"rounds": rounds, "failures": failures}))
"""


class Lab(unittest.TestCase):
    """bolt-lab.sh: the pipeline's stages (record, bolt, smoke, ab) as the lab runs them, against stand-ins."""

    def setUp(self) -> None:
        self.dir = Path(tempfile.mkdtemp())
        self.lab = self.dir / "kiln-lab"
        self.lab.write_text(STANDIN_LAB)
        self.lab.chmod(0o755)
        self.bolt_calls = self.dir / "bolt.calls"
        # A bolt.sh: `same` as SAME says; profile and optimize leave what the real one does, optimize's coverage INS.
        self.bolt = script(
            self.dir / "bolt.sh",
            f"""printf '%s\\n' "$*" >> {self.bolt_calls}
case $1 in
  same) exit "${{SAME:-0}}" ;;
  check) exit 0 ;;
  profile) echo profile > "$3"; if [ -n "${{BOLT_PROFILE_YAML:-}}" ]; then echo yaml > "$BOLT_PROFILE_YAML"; fi; if [ -n "${{MORE:-}}" ]; then echo "BOLT-WARNING: BOLT is estimated to optimize better with ${{MORE}}x more samples. Please consider increasing sampling rate" > "$3.1.log"; fi ;;
  optimize) echo bolted > "$4"; printf 'layout\\ninstructions: %s (-dyno-stats)\\n' "${{INS:-0}}" > "$2.bolt.txt" ;;
esac
""",
        )
        bin_dir = self.dir / "path"
        bin_dir.mkdir()
        # taskset noted, then what it runs run.
        script(bin_dir / "taskset", f'printf "%s\\n" "$*" >> {self.dir}/taskset.calls\nshift 2\nexec "$@"\n')
        self.env = {"UNIT": "none", "BOLT_SH": str(self.bolt), "STANDIN_DIR": str(self.dir), "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMPDIR": str(self.dir / "tmp")}
        self.kiln, self.bolted = self.dir / "kiln", self.dir / "kiln.bolt"
        for b in (self.kiln, self.bolted):
            script(b, "echo kiln 1.2.3\n")

    def tearDown(self) -> None:
        shutil.rmtree(self.dir)

    def go(self, *args: str, **env: str) -> subprocess.CompletedProcess[str]:
        # The lab's own placement unless a test says otherwise: none of the caller's.
        clean = {k: v for k, v in os.environ.items() if k not in ("LAB_CPUS", "SERVER_CPUS", "LAYOUT", "SHARDS", "ROUNDS", "HOGS", "BOLT_RECORD", "UNIT_WEIGHT")}
        return subprocess.run([str(LAB), *args], env={**clean, **self.env, **env}, capture_output=True, text=True, timeout=120)

    def calls(self) -> list[str]:
        return (self.dir / "calls").read_text().splitlines()

    def test_g11_s5_the_ab_runs_calm_then_hogs_in_productions_layout(self) -> None:
        out = self.dir / "ab"
        r = self.go("ab", str(self.kiln), str(self.bolted), str(self.lab), str(out))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.bolt_calls.read_text().splitlines()[0], f"same {self.kiln} {self.bolted}", "the same program first")
        calm, hogs = self.calls()
        common = f"ab --a {self.kiln} --b {self.bolted} --label-a release --label-b bolt --env-a KILN_RT_SHARDS=4 --env-b KILN_RT_SHARDS=4 --profile realistic --rounds 8"
        self.assertEqual(calm, f"{common} --hogs 0 --layout production --server-cpus 16-31 --out {out}/calm/run")
        self.assertEqual(hogs, f"{common} --hogs 1x --layout production --server-cpus 16-31 --out {out}/hogs/run")
        self.assertEqual({l.split()[1] for l in (self.dir / "taskset.calls").read_text().splitlines()}, {"0-15"}, "the lab's clients on their own CPUs")
        self.assertTrue(all(l.startswith("BOLT_RECORD=none ") for l in (self.dir / "envs").read_text().splitlines()), "no profiling in an A/B's rounds")
        read = (out / "read.md").read_text()
        self.assertIn(f"## {out}/calm/run", read)
        self.assertIn(f"## {out}/hogs/run", read)

    def test_g11_s5_an_ab_of_a_build_that_isnt_the_release_laid_out_doesnt_start(self) -> None:
        r = self.go("ab", str(self.kiln), str(self.bolted), str(self.lab), str(self.dir / "ab"), SAME="1")
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse((self.dir / "calls").exists(), "no round run")

    def test_g11_s5_the_placement_is_the_callers_beside_production(self) -> None:
        # adacachy, beside production: the shared layout at the unit's weight, nothing pinned.
        r = self.go("smoke", str(self.kiln), str(self.bolted), str(self.lab), str(self.dir / "s"), LAB_CPUS="", SERVER_CPUS="", LAYOUT="shared")
        self.assertEqual(r.returncode, 0, r.stderr)
        (smoke,) = self.calls()
        self.assertIn("--profile small --rounds 1 --hogs 0 --layout shared --out", smoke)
        self.assertNotIn("--server-cpus", smoke)
        self.assertFalse((self.dir / "taskset.calls").exists())

    def test_g11_s5_the_smoke_passes_only_when_the_bolted_build_runs_as_the_release_does(self) -> None:
        r = self.go("smoke", str(self.kiln), str(self.bolted), str(self.lab), str(self.dir / "s1"))
        self.assertEqual(r.returncode, 0, r.stderr + r.stdout)
        self.assertIn("release and bolt each ran their rounds, errors 0 and 0", r.stdout)
        r = self.go("smoke", str(self.kiln), str(self.bolted), str(self.lab), str(self.dir / "s2"), STANDIN_ERRORS_B="3")
        self.assertEqual(r.returncode, 1)
        self.assertIn("bolt answered 3 requests with errors, release 0", r.stdout)
        r = self.go("smoke", str(self.kiln), str(self.bolted), str(self.lab), str(self.dir / "s3"), STANDIN_FAIL="round 2 (bolt): the server exited")
        self.assertEqual(r.returncode, 1)
        self.assertIn("the server exited", r.stdout)

    def test_g11_s5_record_profiles_every_round_from_the_servers_start_each_call_in_a_folder_of_its_own(self) -> None:
        out = self.dir / "prof"
        for _ in range(2):
            r = self.go("record", str(self.kiln), str(self.lab), str(out), "2")
            self.assertEqual(r.returncode, 0, r.stderr)
        first, second = self.calls()
        self.assertEqual(first, f"run --kiln {self.kiln} --label release --env KILN_RT_SHARDS=4 --profile realistic --rounds 2 --hogs 0 --layout production --server-cpus 16-31 --out {out}/prof-1/run")
        self.assertTrue(second.endswith(f"--out {out}/prof-2/run"))
        envs = (self.dir / "envs").read_text().splitlines()
        self.assertTrue(all(l.startswith("BOLT_RECORD=auto ") for l in envs), "LBR where the CPU has it")
        self.assertTrue(all(l.endswith(" BOLT_FREQ=max") for l in envs), "as often as the kernel lets perf sample")
        self.assertIn("2 bolt.sh: recording the server with its branch stacks", r.stderr, "which perf took, said")
        r = self.go("record", str(self.kiln), str(self.lab), str(out), "0")
        self.assertNotEqual(r.returncode, 0)

    def test_g11_s5_bolt_derives_the_rounds_bolts_two_measures_ask_for(self) -> None:
        out = self.dir / "prof"
        r = self.go("record", str(self.kiln), str(self.lab), str(out), "2")
        self.assertEqual(r.returncode, 0, r.stderr)
        relocs = self.dir / "kiln.relocs"
        relocs.write_text("relocs")
        # 300M instructions in 2 rounds: 150M a round, so the README's 1B takes 7; perf2bolt asks for no more.
        r = self.go("bolt", str(relocs), str(out), INS="300000000")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((out / "rounds.txt").read_text().strip(), "rounds=2 instructions=300000000 more=0 readme=7 bolt=0 needed=7")
        self.assertIn("record ... 5, then bolt again", r.stderr)
        self.assertEqual((out / "kiln.bolt").read_text(), "bolted\n")
        steps = [l.split()[0] for l in self.bolt_calls.read_text().splitlines()]
        self.assertEqual(steps, ["check", "profile", "optimize"], "one pass of perf2bolt, then the layout")
        self.assertEqual((out / "kiln.yaml").read_text(), "yaml\n", "the YAML for later commits from the same pass")
        # perf2bolt's own estimate for a round's recording, 7.5x more samples: 8 rounds, more than the README's.
        r = self.go("bolt", str(relocs), str(out), INS="300000000", MORE="7.5")
        self.assertEqual((out / "rounds.txt").read_text().strip(), "rounds=2 instructions=300000000 more=7.5 readme=7 bolt=8 needed=8")
        # Enough already by both: no more rounds.
        for log in out.glob("kiln.fdata.*.log"):
            log.unlink()
        r = self.go("bolt", str(relocs), str(out), INS="2000000000")
        self.assertEqual((out / "rounds.txt").read_text().strip(), "rounds=2 instructions=2000000000 more=0 readme=1 bolt=0 needed=2")


class Read(unittest.TestCase):
    """bolt-read.py ab: the A/B's numbers and the binary's pages by section, each the median of a build's rounds."""

    def test_g11_s5_the_code_is_every_text_section_and_each_is_read_by_pages_and_windows(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            dd = Path(d)
            lab = dd / "kiln-lab"
            lab.write_text(STANDIN_LAB)
            lab.chmod(0o755)
            run([str(lab), "ab", "--a", "a", "--b", "b", "--label-a", "release", "--label-b", "bolt", "--rounds", "2", "--out", str(dd / "run")], env={"STANDIN_DIR": d})
            r = run([sys.executable, "-I", str(READ), "ab", "release", "bolt", str(dd / "run")])
            self.assertEqual(r.returncode, 0, r.stderr)
            # B's code: .text 4000 + .text.cold 200 + .bolt.org.text 10000 KiB, windows 70 + 10 + 200 of 80 + 150 + 2800.
            self.assertIn("| code (every .text section) | 96,000 | 24,000 | 1,500 of 2,800 | 14,200 | 3,550 | 280 of 3,030 |", r.stdout)
            self.assertIn("| .bolt.org.text | - | - | - | 10,000 | 2,500 | 200 of 2,800 |", r.stdout)
            # BOLT's own: the original .rodata renamed .bolt.org.rodata, its new .rodata and .rodata.cold, together.
            self.assertIn("| .rodata (and .bolt.org.rodata, .rodata.cold) | 6,000 | 1,500 | 200 of 500 | 3,140 | 785 | 94 of 505 |", r.stdout)
            # Medians of rounds: A 9 and 10 ms, B 7 and 8.
            self.assertIn("| server.cpu_ms_per_request | 9.500 | 7.500 | 0.789 |", r.stdout)
            self.assertIn("rounds with sections.txt: release 2, bolt 2", r.stdout)
            bad = run([sys.executable, "-I", str(READ), "ab", "release", "bolt", str(dd / "nothing")])
            self.assertEqual(bad.returncode, 1)
            self.assertEqual(run([sys.executable, "-I", str(READ)]).returncode, 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
