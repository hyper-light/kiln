#!/usr/bin/env python3
"""The projects kiln vendors: take one at a revision, prove each is its upstream plus kiln's
recorded changes, and record new changes (CP-M1; docs/vendoring.md).

  scripts/vendor.py <project> <revision> [--from PATH] [--no-gate]
      Take <project> at <revision> (a full commit id; a release version for Bun): copy its tree,
      check the projects' pins of each other, re-apply kiln's recorded changes, then run the
      gate. A recorded change that no longer applies stops it, naming the change and the files;
      kiln's tree is left as it was, and git's merge waits in a folder of its own.
  scripts/vendor.py <project> --continue [--no-gate] | --abort
      Once the conflict is resolved there: that change as resolved, the rest re-applied, kiln's
      tree written and the gate run. Or the stopped re-vendor dropped.
  scripts/vendor.py check [<project> ...] [--kind git|bun] [--from PROJECT=PATH]
      Prove each vendored tree is its upstream at the pinned revision plus its recorded changes,
      byte for byte, that its UPSTREAM.md lists every change, and that the pins agree.
  scripts/vendor.py record <project> <title> --why <why>
      Record the edits made in a vendored tree as its next change.
  scripts/vendor.py list
      The projects, where they come from, their revisions and recorded changes.

vendor/MANIFEST.toml says what each project is; vendor/patches/<project>/*.patch are its recorded
changes, applied in name order. Upstream sources are fetched into $KILN_VENDOR_CACHE (default
~/.cache/kiln/vendor). Exit status: 0 done, 1 refused or failed (the message says why), 2 usage.

kiln's copy of this file is the original; each hyper-light product carries the same file for what
it vendors itself.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import gzip
import hashlib
import io
import json
import os
import posixpath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Iterator
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # before Python 3.11 (macOS's own python3 is 3.9)
    sys.stderr.write("vendor.py: needs Python 3.11 or later (for tomllib); this is Python %d.%d\n" % sys.version_info[:2])
    raise SystemExit(2) from None

MANIFEST = "vendor/MANIFEST.toml"
PATCHES = "vendor/patches"
FORMAT = 1

# Bounds (master plan §3: every loop ends on a counted budget, memory is charged to a budget).
MAX_FILES = 200_000  # files in one vendored tree
MAX_TREE_BYTES = 4 << 30  # bytes in one vendored tree
MAX_DOWNLOAD = 512 << 20  # bytes in one download
MAX_IMAGE_KEPT = 256 << 20  # bytes of an image's files held while reading it
MAX_LINK_HOPS = 40  # symbolic links followed resolving one path in an image (Linux's ELOOP bound)
MAX_REDIRECTS = 5
HTTP_TIMEOUT_S = 60

# Commands run in the scratch repository where changes are replayed: no user or system
# configuration (signing, hooks, attributes, excludes), fixed identities and dates, so a replay
# is the same everywhere.
WORK_ENV = {
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_AUTHOR_NAME": "kiln vendor",
    "GIT_AUTHOR_EMAIL": "vendor@kiln.invalid",
    "GIT_COMMITTER_NAME": "kiln vendor",
    "GIT_COMMITTER_EMAIL": "vendor@kiln.invalid",
    "GIT_AUTHOR_DATE": "2000-01-01T00:00:00Z",
    "GIT_COMMITTER_DATE": "2000-01-01T00:00:00Z",
    "LC_ALL": "C",
}
# Fetches keep the user's configuration (credentials for private repositories) but never prompt.
FETCH_ENV = {"GIT_TERMINAL_PROMPT": "0", "LC_ALL": "C"}

DIFF_ARGS = ["diff", "--no-color", "--no-ext-diff", "--no-renames", "--binary", "--full-index"]

# path -> (mode, bytes); paths are relative, '/'-separated; mode is 0o644 or 0o755.
Tree = dict[str, tuple[int, bytes]]


class Refused(Exception):
    """A typed refusal: what was refused and why. Nothing changed unless the message says so."""


def say(msg: str) -> None:
    print(f"vendor.py: {msg}", file=sys.stderr)


# ---------------------------------------------------------------------------------------------
# The manifest


@dataclasses.dataclass
class Project:
    name: str
    kind: str
    path: str
    own: list[str]
    gate: list[str]
    spec: dict

    @property
    def revision(self) -> str:
        return self.spec["commit"] if self.kind == "git" else self.spec["version"]

    @property
    def revision_key(self) -> str:
        return "commit" if self.kind == "git" else "version"


@dataclasses.dataclass
class Manifest:
    root: Path
    text: str
    gate: list[str]
    projects: dict[str, Project]


def load_manifest(root: Path) -> Manifest:
    path = root / MANIFEST
    try:
        text = path.read_text(encoding="utf-8")
        data = tomllib.loads(text)
    except FileNotFoundError:
        raise Refused(f"{MANIFEST} is missing under {root}") from None
    except tomllib.TOMLDecodeError as e:
        raise Refused(f"{MANIFEST} is not valid TOML: {e}") from None
    if data.get("format") != FORMAT:
        raise Refused(f"{MANIFEST}: format {data.get('format')!r}; this vendor.py reads format {FORMAT}")
    projects: dict[str, Project] = {}
    for name, spec in (data.get("projects") or {}).items():
        if not re.fullmatch(r"[a-z0-9][a-z0-9_-]*", name):
            raise Refused(f"{MANIFEST}: project name {name!r}: lowercase letters, digits, - and _ only")
        kind = spec.get("kind")
        need = {"git": ("repository", "commit", "path"), "bun": ("version", "path", "image", "image_tag", "image_digest")}
        if kind not in need:
            raise Refused(f"{MANIFEST}: [projects.{name}] kind {kind!r}: git or bun")
        missing = [k for k in need[kind] if not spec.get(k)]
        if missing:
            raise Refused(f"{MANIFEST}: [projects.{name}] lacks {', '.join(missing)}")
        if kind == "git" and not is_commit_id(spec["commit"]):
            raise Refused(f"{MANIFEST}: [projects.{name}] commit {spec['commit']!r} is not a full commit id")
        rel = spec["path"]
        if not rel.startswith("vendor/") or ".." in rel.split("/") or rel.endswith("/"):
            raise Refused(f"{MANIFEST}: [projects.{name}] path {rel!r} must be a folder under vendor/")
        projects[name] = Project(
            name=name,
            kind=kind,
            path=rel,
            own=list(spec.get("own", [])),
            gate=list(spec.get("gate", [])),
            spec=spec,
        )
    return Manifest(root=root, text=text, gate=list(data.get("gate", [])), projects=projects)


def set_manifest_value(text: str, project: str, key: str, old: str, new: str) -> str:
    """The manifest's text with `key = "old"` in [projects.<project>] set to `new`, the rest of
    the file (comments, order, spacing) untouched."""
    lines = text.splitlines(keepends=True)
    header = f"[projects.{project}]"
    inside = False
    hits = []
    for i, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("["):
            inside = stripped == header
            continue
        if inside and re.match(rf'{re.escape(key)}\s*=\s*"{re.escape(old)}"', stripped):
            hits.append(i)
    if len(hits) != 1:
        raise Refused(f'{MANIFEST}: expected one `{key} = "{old}"` in {header}, found {len(hits)}')
    i = hits[0]
    lines[i] = lines[i].replace(f'"{old}"', f'"{new}"', 1)
    return "".join(lines)


def is_commit_id(s: str) -> bool:
    return bool(re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", s))


# ---------------------------------------------------------------------------------------------
# Trees on disk


def run(args: list[str], *, cwd: Path | None = None, env: dict | None = None, input: bytes | None = None) -> subprocess.CompletedProcess:
    full = dict(os.environ)
    if env:
        full.update(env)
    return subprocess.run(args, cwd=cwd, env=full, input=input, capture_output=True, check=False)


def git(args: list[str], *, cwd: Path | None = None, env: dict | None = None, input: bytes | None = None, ok: bool = False) -> subprocess.CompletedProcess:
    r = run(["git", *args], cwd=cwd, env=env, input=input)
    if r.returncode != 0 and not ok:
        raise Refused(f"git {' '.join(args)} failed: {r.stderr.decode(errors='replace').strip()}")
    return r


def mode_of(st_mode: int) -> int:
    return 0o755 if st_mode & 0o111 else 0o644


def read_vendored(root: Path, project: Project) -> Tree:
    """The project's folder as git would commit it (tracked and untracked files, ignored ones
    left out), less kiln's own files. Where the root isn't a git work tree (kiln's pipelines check
    a commit out without its .git), the folder's files as they are."""
    folder = root / project.path
    if not folder.is_dir():
        return {}
    own = set(project.own)
    inside = git(["rev-parse", "--is-inside-work-tree"], cwd=root, ok=True)
    if inside.returncode != 0 or inside.stdout.strip() != b"true":
        return {rel: entry for rel, entry in read_folder(folder).items() if rel not in own}
    r = git(["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", project.path], cwd=root)
    tree: Tree = {}
    total = 0
    prefix = project.path + "/"
    for raw in r.stdout.split(b"\0"):
        if not raw:
            continue
        repo_path = raw.decode("utf-8", errors="surrogateescape")
        if not repo_path.startswith(prefix):
            continue
        rel = repo_path[len(prefix):]
        if rel in own:
            continue
        p = root / repo_path
        if not p.exists() and not p.is_symlink():
            continue  # deleted from the working tree
        if p.is_symlink():
            raise Refused(f"{repo_path} is a symbolic link: vendored trees hold plain files only")
        data = p.read_bytes()
        total += len(data)
        if len(tree) >= MAX_FILES or total > MAX_TREE_BYTES:
            raise Refused(f"{project.path} is over the bounds of a vendored tree ({MAX_FILES} files, {MAX_TREE_BYTES} bytes)")
        mode = mode_of(p.stat().st_mode) if os.name != "nt" else 0o644
        tree[rel] = (mode, data)
    return tree


def read_folder(folder: Path, skip: frozenset[str] = frozenset({".git"})) -> Tree:
    """Every file under folder (the scratch repository's working tree)."""
    tree: Tree = {}
    total = 0
    for dirpath, dirnames, filenames in os.walk(folder):
        rel_dir = Path(dirpath).relative_to(folder).as_posix()
        dirnames[:] = sorted(d for d in dirnames if not (rel_dir == "." and d in skip))
        for f in sorted(filenames):
            p = Path(dirpath) / f
            rel = f if rel_dir == "." else f"{rel_dir}/{f}"
            if p.is_symlink():
                raise Refused(f"{rel} is a symbolic link: vendored trees hold plain files only")
            data = p.read_bytes()
            total += len(data)
            if len(tree) >= MAX_FILES or total > MAX_TREE_BYTES:
                raise Refused(f"a tree over the bounds of a vendored tree ({MAX_FILES} files, {MAX_TREE_BYTES} bytes)")
            tree[rel] = (mode_of(p.stat().st_mode) if os.name != "nt" else 0o644, data)
    return tree


def write_into(folder: Path, old: Tree, new: Tree) -> list[str]:
    """Make folder's files `new` where they are `old` now: remove what's gone, write what's new
    or changed (unchanged files keep their times), and remove folders left empty. Files not in
    `old` (kiln's own) are untouched. Returns the paths written or removed."""
    touched = []
    for rel in sorted(set(old) - set(new)):
        (folder / rel).unlink()
        touched.append(rel)
    for rel, (mode, data) in sorted(new.items()):
        if old.get(rel) == (mode, data):
            continue
        p = folder / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        if not p.exists() or p.read_bytes() != data:
            p.write_bytes(data)
        if os.name != "nt":
            p.chmod(mode)
        touched.append(rel)
    for dirpath, dirnames, filenames in os.walk(folder, topdown=False):
        d = Path(dirpath)
        if d != folder and not any(d.iterdir()):
            d.rmdir()
    return touched


def compare(expected: Tree, actual: Tree) -> list[str]:
    """How actual differs from expected, one line per path."""
    out = []
    for rel in sorted(set(expected) | set(actual)):
        e, a = expected.get(rel), actual.get(rel)
        if e == a:
            continue
        if a is None:
            out.append(f"missing here: {rel}")
        elif e is None:
            out.append(f"only here: {rel}")
        elif e[1] != a[1]:
            out.append(f"differs: {rel}")
        else:
            out.append(f"mode {oct(a[0])} here, {oct(e[0])} upstream: {rel}")
    return out


# ---------------------------------------------------------------------------------------------
# The cache


def cache_dir() -> Path:
    c = Path(os.environ.get("KILN_VENDOR_CACHE") or Path.home() / ".cache" / "kiln" / "vendor")
    c.mkdir(parents=True, exist_ok=True)
    return c


@contextlib.contextmanager
def locked(path: Path) -> Iterator[None]:
    """One vendor.py at a time per cached repository (fetches into one repository race)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a+b") as f:
        try:
            import fcntl
        except ImportError:  # Windows: no advisory locks; runs there are one at a time by hand
            yield
            return
        fcntl.flock(f, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(f, fcntl.LOCK_UN)


# ---------------------------------------------------------------------------------------------
# git projects


class GitUpstream:
    def __init__(self, project: Project, source: str | None):
        self.project = project
        self.repository = project.spec["repository"]
        self.source = source
        self.bare = cache_dir() / "git" / f"{project.name}.git"

    def _has(self, commit: str) -> bool:
        return git(["--git-dir", str(self.bare), "cat-file", "-e", f"{commit}^{{commit}}"], ok=True).returncode == 0

    def fetch(self, commit: str) -> None:
        if not is_commit_id(commit):
            raise Refused(f"{self.project.name}: {commit!r} is not a full commit id: give all 40 characters")
        with locked(self.bare.with_suffix(".lock")):
            if not self.bare.exists():
                git(["init", "-q", "--bare", str(self.bare)])
            if self._has(commit):
                return
            tried = []
            for url in [u for u in (self.source, self.repository) if u]:
                # Servers that serve a commit by its id: just that commit.
                git(["--git-dir", str(self.bare), "fetch", "-q", "--depth", "1", "--no-tags", url, commit], env=FETCH_ENV, ok=True)
                if self._has(commit):
                    return
                # Otherwise (a local path, a server that refuses ids): every branch and tag.
                r = git(
                    ["--git-dir", str(self.bare), "fetch", "-q", "--no-tags", url, "+refs/heads/*:refs/remotes/src/*", "+refs/tags/*:refs/tags/*"],
                    env=FETCH_ENV,
                    ok=True,
                )
                if self._has(commit):
                    return
                why = r.stderr.decode(errors="replace").strip().splitlines()
                tried.append(f"{url} ({why[-1] if why else 'no such commit'})")
            hint = "" if self.source else "; a commit not yet pushed comes --from a clone that has it"
            raise Refused(f"{self.project.name}: no commit {commit} in {', '.join(tried)}{hint}")

    def tree(self, commit: str) -> Tree:
        """The files kiln takes from upstream's commit, at the paths the manifest names."""
        self.fetch(commit)
        take = list(self.project.spec.get("take") or [])
        exclude = list(self.project.spec.get("exclude") or [])
        pathspec = [] if not take or take == ["."] else ["--", *take]
        r = git(["--git-dir", str(self.bare), "ls-tree", "-r", "-z", "--full-tree", commit, *pathspec])
        wanted: list[tuple[str, int, str]] = []
        for raw in r.stdout.split(b"\0"):
            if not raw:
                continue
            meta, _, path_b = raw.partition(b"\t")
            mode, kind, sha = meta.decode().split(" ")
            path = path_b.decode("utf-8", errors="surrogateescape")
            if any(path == e or path.startswith(e.rstrip("/") + "/") for e in exclude):
                continue
            if kind == "commit":
                raise Refused(f"{self.project.name}: {path} is a git submodule at {commit}: vendor it as a project of its own")
            if mode == "120000":
                raise Refused(f"{self.project.name}: {path} is a symbolic link at {commit}: vendored trees hold plain files only; exclude it")
            if mode not in ("100644", "100755"):
                raise Refused(f"{self.project.name}: {path} has mode {mode} at {commit}")
            wanted.append((path, 0o755 if mode == "100755" else 0o644, sha))
        for t in take:
            if t != "." and not any(p == t or p.startswith(t.rstrip("/") + "/") for p, _, _ in wanted):
                raise Refused(f"{self.project.name}: take path {t!r} is not in {commit}")
        if len(wanted) > MAX_FILES:
            raise Refused(f"{self.project.name}: {len(wanted)} files at {commit}, over the bound of {MAX_FILES}")
        blobs = cat_blobs(self.bare, [sha for _, _, sha in wanted])
        return {path: (mode, blobs[sha]) for path, mode, sha in wanted}

    def show(self, commit: str, path: str) -> bytes | None:
        r = git(["--git-dir", str(self.bare), "show", f"{commit}:{path}"], ok=True)
        return r.stdout if r.returncode == 0 else None

    def files(self, commit: str, folder: str) -> dict[str, bytes]:
        """Each file under folder at commit, by its name relative to folder."""
        r = git(["--git-dir", str(self.bare), "ls-tree", "-r", "-z", "--full-tree", commit, "--", folder], ok=True)
        entries = []
        for raw in r.stdout.split(b"\0"):
            meta, _, path_b = raw.partition(b"\t")
            if raw and meta.split(b" ")[1:2] == [b"blob"]:
                entries.append((path_b.decode("utf-8", errors="surrogateescape"), meta.split(b" ")[2].decode()))
        blobs = cat_blobs(self.bare, [sha for _, sha in entries]) if entries else {}
        return {path.removeprefix(folder.rstrip("/") + "/"): blobs[sha] for path, sha in entries}


def cat_blobs(bare: Path, shas: list[str]) -> dict[str, bytes]:
    """Blobs by id, read in one `git cat-file --batch`."""
    unique = sorted(set(shas))
    r = git(["--git-dir", str(bare), "cat-file", "--batch"], input=("\n".join(unique) + "\n").encode())
    out: dict[str, bytes] = {}
    buf = r.stdout
    pos = 0
    total = 0
    for _ in unique:
        nl = buf.find(b"\n", pos)
        if nl < 0:
            raise Refused("git cat-file: its output ended early")
        header = buf[pos:nl].decode().split(" ")
        if len(header) != 3 or header[1] != "blob":
            raise Refused(f"git cat-file: unexpected {buf[pos:nl]!r}")
        size = int(header[2])
        total += size
        if total > MAX_TREE_BYTES:
            raise Refused(f"upstream's files are over the bound of {MAX_TREE_BYTES} bytes")
        start = nl + 1
        out[header[0]] = buf[start : start + size]
        pos = start + size + 1
    return out


# ---------------------------------------------------------------------------------------------
# Recorded changes


@dataclasses.dataclass
class Change:
    file: Path
    header: str  # the lines before the diff: Subject:, Why:
    diff: bytes

    @property
    def name(self) -> str:
        return self.file.name

    @property
    def title(self) -> str:
        m = re.search(r"^Subject: (.*)$", self.header, re.M)
        return m.group(1).strip() if m else self.file.stem


def load_changes(root: Path, project: Project) -> list[Change]:
    folder = root / PATCHES / project.name
    if not folder.is_dir():
        return []
    changes = []
    for f in sorted(folder.glob("*.patch")):
        data = f.read_bytes()
        at = data.find(b"diff --git ")
        if at < 0:
            raise Refused(f"{f.relative_to(root)} holds no diff")
        changes.append(Change(file=f, header=data[:at].decode("utf-8"), diff=data[at:]))
    return changes


class Scratch:
    """A scratch git repository where upstream trees are committed and changes replayed."""

    def __init__(self) -> None:
        parent = cache_dir() / "work"
        parent.mkdir(parents=True, exist_ok=True)
        self.dir = Path(tempfile.mkdtemp(prefix="replay-", dir=parent))
        self.git(["init", "-q", "-b", "upstream"])
        # Every file's bytes as they are: no line-ending, filter or encoding conversion, whatever
        # .gitattributes the upstream tree carries.
        (self.dir / ".git" / "info").mkdir(parents=True, exist_ok=True)
        (self.dir / ".git" / "info" / "attributes").write_text("* -text -ident -filter -working-tree-encoding\n")
        # Kept: a conflict waits in it for the person (a re-vendor's `--continue`), so it outlives
        # the run that made it.
        self.kept = False

    @classmethod
    def adopt(cls, where: Path) -> "Scratch":
        """The scratch repository a stopped re-vendor kept."""
        s = cls.__new__(cls)
        s.dir = where
        s.kept = True
        return s

    def close(self) -> None:
        shutil.rmtree(self.dir, ignore_errors=True)

    def git(self, args: list[str], *, input: bytes | None = None, ok: bool = False) -> subprocess.CompletedProcess:
        return git(["-C", str(self.dir), *args], env=WORK_ENV, input=input, ok=ok)

    def set_tree(self, tree: Tree) -> None:
        for entry in self.dir.iterdir():
            if entry.name == ".git":
                continue
            if entry.is_dir() and not entry.is_symlink():
                shutil.rmtree(entry)
            else:
                entry.unlink()
        for rel, (mode, data) in tree.items():
            p = self.dir / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(data)
            if os.name != "nt":
                p.chmod(mode)

    def commit(self, message: str) -> str:
        self.git(["add", "--all", "--force", "."])
        self.git(["commit", "-q", "--allow-empty", "--no-verify", "-m", message])
        return self.git(["rev-parse", "HEAD"]).stdout.decode().strip()

    def apply(self, change: Change) -> subprocess.CompletedProcess:
        return self.git(["apply", "--index", "--whitespace=nowarn", "-"], input=change.diff, ok=True)

    def diff(self, a: str, b: str) -> bytes:
        return self.git([*DIFF_ARGS, a, b]).stdout

    def read(self) -> Tree:
        return read_folder(self.dir)


def replay(project: Project, upstream: Tree, changes: list[Change], revision: str) -> tuple[Scratch, list[str]]:
    """upstream, then each change on it, committed in order: the scratch repository and each
    change's commit. A change that doesn't apply to the revision it was recorded on is refused."""
    s = Scratch()
    try:
        s.set_tree(upstream)
        s.commit(f"{project.name} at {revision}")
        commits = []
        for c in changes:
            r = s.apply(c)
            if r.returncode != 0:
                raise Refused(
                    f"{project.name}: recorded change {c.name} ({c.title}) does not apply to upstream at {revision}: "
                    f"{r.stderr.decode(errors='replace').strip()}"
                )
            commits.append(s.commit(c.title))
        return s, commits
    except BaseException:
        s.close()
        raise


def missing_from_notes(root: Path, project: Project, changes: list[Change]) -> list[str]:
    """Changes the project's UPSTREAM.md doesn't name (each change is a row there)."""
    if not changes:
        return []
    notes = root / project.path / "UPSTREAM.md"
    if "UPSTREAM.md" not in project.own or not notes.exists():
        return [f"{project.path}/UPSTREAM.md is missing: it lists each recorded change"]
    text = notes.read_text(encoding="utf-8")
    return [f"{project.path}/UPSTREAM.md doesn't name {c.name}" for c in changes if c.name not in text]


# ---------------------------------------------------------------------------------------------
# Bun


@dataclasses.dataclass
class BunFacts:
    version: str
    image_digest: str
    alpine: str
    packages: dict[str, str]  # apk package -> version, e.g. musl -> 1.2.6-r2
    vars: dict[str, str]


def upstream_version(apk_version: str) -> str:
    return re.sub(r"-r\d+$", "", apk_version)


def http_get(url: str, headers: dict[str, str] | None = None, limit: int = MAX_DOWNLOAD) -> tuple[bytes, dict[str, str]]:
    """GET url, following redirects by hand (an Authorization header never follows to another
    host), bounded in size."""
    headers = dict(headers or {})
    for _ in range(MAX_REDIRECTS + 1):
        req = urllib.request.Request(url, headers=headers, method="GET")
        opener = urllib.request.build_opener(NoRedirect)
        try:
            with opener.open(req, timeout=HTTP_TIMEOUT_S) as resp:
                data = resp.read(limit + 1)
                if len(data) > limit:
                    raise Refused(f"{url}: over the download bound of {limit} bytes")
                return data, {k.lower(): v for k, v in resp.headers.items()}
        except urllib.error.HTTPError as e:
            if e.code in (301, 302, 303, 307, 308) and e.headers.get("Location"):
                nxt = urllib.parse.urljoin(url, e.headers["Location"])
                if urllib.parse.urlsplit(nxt).netloc != urllib.parse.urlsplit(url).netloc:
                    headers.pop("Authorization", None)
                url = nxt
                continue
            raise HttpError(url, e.code, {k.lower(): v for k, v in e.headers.items()}) from None
        except urllib.error.URLError as e:
            raise Refused(f"{url}: {e.reason}") from None
    raise Refused(f"{url}: more than {MAX_REDIRECTS} redirects")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D401 (urllib's API)
        return None


class HttpError(Refused):
    def __init__(self, url: str, code: int, headers: dict[str, str]):
        super().__init__(f"{url}: HTTP {code}")
        self.code = code
        self.headers = headers


class Registry:
    """An OCI registry, read anonymously: manifests and blobs by digest, each verified."""

    ACCEPT = ", ".join(
        [
            "application/vnd.oci.image.index.v1+json",
            "application/vnd.docker.distribution.manifest.list.v2+json",
            "application/vnd.oci.image.manifest.v1+json",
            "application/vnd.docker.distribution.manifest.v2+json",
        ]
    )

    def __init__(self, image: str):
        host, _, repo = image.partition("/")
        if not repo:
            raise Refused(f"image {image!r}: host/repository")
        self.base = "https://registry-1.docker.io" if host == "docker.io" else f"https://{host}"
        self.repo = repo
        self.token: str | None = None

    def _get(self, path: str, accept: str | None = None) -> tuple[bytes, dict[str, str]]:
        url = f"{self.base}/v2/{self.repo}/{path}"
        for _ in range(2):
            headers = {"Accept": accept} if accept else {}
            if self.token:
                headers["Authorization"] = f"Bearer {self.token}"
            try:
                return http_get(url, headers)
            except HttpError as e:
                if e.code != 401 or self.token:
                    raise
                self.token = self._token(e.headers.get("www-authenticate", ""))
        raise Refused(f"{url}: refused after authenticating")

    def _token(self, challenge: str) -> str:
        if not challenge.lower().startswith("bearer "):
            raise Refused(f"registry {self.base}: unsupported authentication {challenge!r}")
        params = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
        realm = params.pop("realm", None)
        if not realm:
            raise Refused(f"registry {self.base}: no token realm in {challenge!r}")
        params.setdefault("scope", f"repository:{self.repo}:pull")
        data, _ = http_get(realm + "?" + urllib.parse.urlencode(params), limit=1 << 20)
        body = json.loads(data)
        token = body.get("token") or body.get("access_token")
        if not token:
            raise Refused(f"registry {self.base}: no token from {realm}")
        return token

    def manifest(self, ref: str) -> tuple[dict, str]:
        data, headers = self._get(f"manifests/{ref}", self.ACCEPT)
        digest = "sha256:" + hashlib.sha256(data).hexdigest()
        if ref.startswith("sha256:") and digest != ref:
            raise Refused(f"{self.repo}@{ref}: the registry's manifest has digest {digest}")
        return json.loads(data), digest

    def blob(self, digest: str) -> bytes:
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            raise Refused(f"blob digest {digest!r}")
        cached = cache_dir() / "blobs" / "sha256" / digest.removeprefix("sha256:")
        if cached.exists():
            data = cached.read_bytes()
            if "sha256:" + hashlib.sha256(data).hexdigest() == digest:
                return data
        data, _ = self._get(f"blobs/{digest}")
        if "sha256:" + hashlib.sha256(data).hexdigest() != digest:
            raise Refused(f"{self.repo}: blob {digest} came back with other bytes")
        cached.parent.mkdir(parents=True, exist_ok=True)
        tmp = cached.with_suffix(".part")
        tmp.write_bytes(data)
        tmp.replace(cached)
        return data


class ImageFS:
    """An image's file system, its layers applied in order (OCI image-spec layer rules: whiteouts
    and opaque directories), keeping the bytes of files under `keep` only."""

    def __init__(self, keep: tuple[str, ...]):
        self.keep = keep
        self.files: dict[str, tuple[int, bytes | None]] = {}
        self.links: dict[str, str] = {}
        self.kept = 0

    def _drop(self, path: str, subtree_only: bool) -> None:
        prefix = path + "/"
        for table in (self.files, self.links):
            for k in [k for k in table if k.startswith(prefix) or (not subtree_only and k == path)]:
                del table[k]

    def apply(self, blob: bytes, media_type: str) -> None:
        if media_type.endswith("+zstd") or media_type.endswith(".zstd"):
            try:
                from compression import zstd  # Python 3.14
            except ImportError:
                raise Refused("a zstd-compressed image layer needs Python 3.14 or later") from None
            raw = zstd.decompress(blob)
        elif blob[:2] == b"\x1f\x8b":
            raw = gzip.decompress(blob)
        else:
            raw = blob
        whiteouts: list[tuple[str, bool]] = []
        adds: list[tuple[str, str, int, bytes | str]] = []
        with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as tf:
            for m in tf:
                path = posixpath.normpath(m.name.lstrip("/")).removeprefix("./")
                if path in (".", ""):
                    continue
                base, parent = posixpath.basename(path), posixpath.dirname(path)
                if base == ".wh..wh..opq":
                    whiteouts.append((parent, True))
                elif base.startswith(".wh."):
                    whiteouts.append((posixpath.join(parent, base[4:]), False))
                elif m.issym():
                    adds.append((path, "link", 0, m.linkname))
                elif m.islnk():
                    adds.append((path, "hard", 0, posixpath.normpath(m.linkname.lstrip("/"))))
                elif m.isfile():
                    # Files outside the kept folders are known by name only: reading one refuses.
                    data: bytes | None = None
                    if any(path.startswith(k + "/") or path == k for k in self.keep):
                        f = tf.extractfile(m)
                        if f is None:
                            raise Refused(f"layer member {path} has no data")
                        data = f.read()
                        self.kept += len(data)
                        if self.kept > MAX_IMAGE_KEPT:
                            raise Refused(f"over the bound of {MAX_IMAGE_KEPT} bytes held reading an image")
                    adds.append((path, "file", mode_of(m.mode), data))
                elif m.isdir():
                    adds.append((path, "dir", 0, b""))
        # A layer's whiteouts hide what lower layers hold, never what the layer itself adds.
        for path, opaque in whiteouts:
            self._drop(path, subtree_only=opaque)
        for path, kind, mode, value in adds:
            if kind == "dir":
                # A folder replaces a lower layer's file or link at its path, and keeps what's under it.
                self.files.pop(path, None)
                self.links.pop(path, None)
                continue
            self._drop(path, subtree_only=False)
            if kind == "link":
                self.links[path] = value  # type: ignore[assignment]
            elif kind == "hard":
                target = self.files.get(value)  # type: ignore[arg-type]
                if target is not None:
                    self.files[path] = target
            else:
                self.files[path] = (mode, value)  # type: ignore[assignment]

    def resolve(self, path: str) -> str:
        """path with every symbolic link in it followed, within MAX_LINK_HOPS."""
        parts = [p for p in path.strip("/").split("/") if p]
        done: list[str] = []
        hops = 0
        i = 0
        while i < len(parts):
            part = parts[i]
            if part == ".":
                i += 1
                continue
            if part == "..":
                done = done[:-1]
                i += 1
                continue
            here = "/".join([*done, part])
            target = self.links.get(here)
            if target is None:
                done.append(part)
                i += 1
                continue
            hops += 1
            if hops > MAX_LINK_HOPS:
                raise Refused(f"{path}: more than {MAX_LINK_HOPS} symbolic links")
            rest = parts[i + 1 :]
            if target.startswith("/"):
                done = []
            parts = [p for p in target.split("/") if p] + rest
            i = 0
        return "/".join(done)

    def read(self, path: str) -> tuple[int, bytes]:
        real = self.resolve(path)
        got = self.files.get(real)
        if got is None:
            raise Refused(f"{path} is not a file in the image (resolved to /{real})")
        mode, data = got
        if data is None:
            raise Refused(f"{path} resolves to /{real}, outside the folders kept reading the image ({', '.join(self.keep)})")
        return mode, data


def apk_versions(db: bytes) -> dict[str, str]:
    """Package -> version, from an Alpine image's /lib/apk/db/installed."""
    out: dict[str, str] = {}
    name = None
    for line in db.decode("utf-8", errors="replace").splitlines():
        if line.startswith("P:"):
            name = line[2:]
        elif line.startswith("V:") and name:
            out[name] = line[2:]
        elif not line:
            name = None
    return out


def render_bun_json(d: dict) -> bytes:
    """bun.json in its one layout: one line per platform, the musl runtime's hashes a line each."""
    q = lambda s: json.dumps(s, ensure_ascii=False)  # noqa: E731
    lines = ["{"]
    lines.append(f'  "comment": {q(d["comment"])},')
    lines.append(f'  "version": {q(d["version"])},')
    lines.append(f'  "url": {q(d["url"])},')
    lines.append('  "platforms": {')
    plats = list(d["platforms"].items())
    for n, (name, p) in enumerate(plats):
        inner = ", ".join(f"{q(k)}: {q(v)}" for k, v in p.items())
        lines.append(f"    {q(name)}: {{ {inner} }}" + ("," if n < len(plats) - 1 else ""))
    lines.append("  },")
    lines.append('  "musl": {')
    musl = d["musl"]
    keys = list(musl.keys())
    for n, key in enumerate(keys):
        value = musl[key]
        comma = "," if n < len(keys) - 1 else ""
        if isinstance(value, str):
            lines.append(f"    {q(key)}: {q(value)}{comma}")
        elif isinstance(value, list):
            lines.append(f"    {q(key)}: [{', '.join(q(v) for v in value)}]{comma}")
        else:
            lines.append(f"    {q(key)}: {{")
            items = list(value.items())
            for m, (k, v) in enumerate(items):
                lines.append(f"      {q(k)}: {q(v)}" + ("," if m < len(items) - 1 else ""))
            lines.append(f"    }}{comma}")
    lines.append("  }")
    lines.append("}")
    return ("\n".join(lines) + "\n").encode("utf-8")


def notice_strings(f: BunFacts) -> list[str]:
    """What vendor/bun/NOTICE says of the versions it covers."""
    musl = f.vars["musl"]
    gcc = f.vars["gcc"]
    branch = ".".join(f.alpine.split(".")[:2])
    return [
        f"Bun {f.version}",
        f"bun-v{f.version}",
        f"Alpine Linux {f.alpine}",
        f"musl {musl}",
        f"musl-{f.packages['musl']}",
        f"musl-{musl}.tar.gz",
        f"GCC {gcc}",
        f"libstdc++-{f.packages['libstdc++']}",
        f"libgcc-{f.packages['libgcc']}",
        f"gcc-{gcc}",
        f"{branch}-stable",
    ]


class BunUpstream:
    """Bun's release as kiln carries it (vendor/bun/README.md): the musl runtime from Oven's
    Alpine image, the licenses from their sources, bun.json's hashes from the release."""

    def __init__(self, project: Project, root: Path):
        self.project = project
        self.root = root

    def tree(self, version: str) -> tuple[Tree, BunFacts]:
        spec = self.project.spec
        if not re.fullmatch(r"\d+\.\d+\.\d+", version):
            raise Refused(f"bun: version {version!r}: major.minor.patch")
        reg = Registry(spec["image"])
        tag = spec["image_tag"].format(version=version)
        if version == spec["version"]:
            index, digest = reg.manifest(spec["image_digest"])
        else:
            index, digest = reg.manifest(tag)
        if "manifests" not in index:
            raise Refused(f"{spec['image']}:{tag} is not a multi-platform image index")
        musl_spec = spec.get("musl") or {}
        keep: set[str] = {"etc", "lib/apk/db"}
        for arch in musl_spec.values():
            for src in arch["files"].values():
                d = posixpath.dirname(src.strip("/"))
                keep |= {d, d.removeprefix("usr/"), "usr/" + d.removeprefix("usr/")}
        tree: Tree = {}
        seen: list[tuple[str, dict[str, str]]] = []
        for arch, a in musl_spec.items():
            os_, _, cpu = a["platform"].partition("/")
            match = [m for m in index["manifests"] if m.get("platform", {}).get("os") == os_ and m.get("platform", {}).get("architecture") == cpu]
            if len(match) != 1:
                raise Refused(f"{spec['image']}@{digest}: {len(match)} manifests for {a['platform']}")
            manifest, _ = reg.manifest(match[0]["digest"])
            fs = ImageFS(tuple(sorted(keep)))
            for layer in manifest.get("layers", []):
                fs.apply(reg.blob(layer["digest"]), layer.get("mediaType", ""))
            for name, src in a["files"].items():
                tree[f"musl/{arch}/{name}"] = fs.read(src)
            alpine = fs.read("/etc/alpine-release")[1].decode().strip()
            seen.append((alpine, apk_versions(fs.read("/lib/apk/db/installed")[1])))
        if not seen:
            raise Refused("bun: the manifest names no musl runtime ([projects.bun.musl.<arch>])")
        alpine, pkgs = seen[0]
        for other_alpine, other in seen[1:]:
            for p in ("musl", "libstdc++", "libgcc"):
                if other_alpine != alpine or other.get(p) != pkgs.get(p):
                    raise Refused(f"bun {version}: the image's platforms differ in Alpine or {p}")
        for p in ("musl", "libstdc++", "libgcc"):
            if p not in pkgs:
                raise Refused(f"bun {version}: the image has no {p} package")
        packages = {p: pkgs[p] for p in ("musl", "libstdc++", "libgcc")}
        facts = BunFacts(
            version=version,
            image_digest=digest,
            alpine=alpine,
            packages=packages,
            vars={"version": version, "alpine": alpine, "musl": upstream_version(packages["musl"]), "gcc": upstream_version(packages["libstdc++"])},
        )
        for rel, url in (spec.get("licenses") or {}).items():
            data, _ = http_get(url.format(**facts.vars))
            tree[rel] = (0o644, data)
        shasums_url = spec.get("shasums", "https://github.com/oven-sh/bun/releases/download/bun-v{version}/SHASUMS256.txt").format(**facts.vars)
        sums_text, _ = http_get(shasums_url, limit=1 << 20)
        sums = {}
        for line in sums_text.decode().splitlines():
            parts = line.split()
            if len(parts) == 2:
                sums[parts[1].lstrip("*")] = parts[0]
        current = json.loads((self.root / self.project.path / "bun.json").read_bytes())
        for name, plat in current["platforms"].items():
            file = plat["file"]
            if file not in sums:
                raise Refused(f"bun {version}: {shasums_url} lists no {file} (platform {name})")
            plat["sha256"] = sums[file]
        current["version"] = version
        current["musl"]["alpine"] = alpine
        current["musl"]["packages"] = [f"{p}-{v}" for p, v in packages.items()]
        for arch, a in musl_spec.items():
            current["musl"][arch] = {name: hashlib.sha256(tree[f"musl/{arch}/{name}"][1]).hexdigest() for name in a["files"]}
        tree["bun.json"] = (0o644, render_bun_json(current))
        return tree, facts

    def notice_problems(self, facts: BunFacts) -> list[str]:
        notice = self.root / self.project.path / "NOTICE"
        if "NOTICE" not in self.project.own or not notice.exists():
            return []
        text = notice.read_text(encoding="utf-8")
        return [f"{self.project.path}/NOTICE doesn't say {s!r}" for s in notice_strings(facts) if s not in text]

    def update_notice(self, old: BunFacts, new: BunFacts) -> None:
        notice = self.root / self.project.path / "NOTICE"
        if not notice.exists():
            return
        text = notice.read_text(encoding="utf-8")
        for a, b in zip(notice_strings(old), notice_strings(new)):
            text = text.replace(a, b)
        notice.write_text(text, encoding="utf-8")


# ---------------------------------------------------------------------------------------------
# The operations


@dataclasses.dataclass
class Expected:
    tree: Tree
    scratch: Scratch | None
    commits: list[str]
    changes: list[Change]
    upstream: GitUpstream | BunUpstream
    facts: BunFacts | None


def expected_tree(root: Path, project: Project, revision: str, source: str | None) -> Expected:
    """What the project's folder holds at revision: upstream plus the recorded changes."""
    if project.kind == "git":
        up = GitUpstream(project, source)
        base = up.tree(revision)
        changes = load_changes(root, project)
        scratch, commits = replay(project, base, changes, revision)
        return Expected(scratch.read(), scratch, commits, changes, up, None)
    up_b = BunUpstream(project, root)
    tree, facts = up_b.tree(revision)
    if load_changes(root, project):
        raise Refused(f"bun: its files are upstream's or kiln's own ({', '.join(project.own)}); it takes no recorded changes")
    return Expected(tree, None, [], [], up_b, facts)


def is_product(repository: str) -> bool:
    return repository.startswith("https://github.com/hyper-light/")


def takes(spec: dict, path: str) -> bool:
    """Whether a git project's take and exclude lists bring path (or anything under it) in."""
    take = list(spec.get("take") or [])
    exclude = [e.rstrip("/") for e in spec.get("exclude") or []]
    if any(path == e or path.startswith(e + "/") for e in exclude):
        return False
    return not take or take == ["."] or any(path == t or path.startswith(t.rstrip("/") + "/") or t.startswith(path + "/") for t in take)


def pin_problems(manifest: Manifest, project: Project, revision: str, upstream: GitUpstream | BunUpstream) -> list[str]:
    """kiln holds one copy of each project, so a project that vendors others (a hyper-light
    product) must agree with kiln on each of them:
    - one kiln vendors too it takes at kiln's revision with the same recorded changes, and kiln's
      copy of the product leaves the product's copy of it out (the product depends on it through
      `[patch]`, which kiln points at its own copy);
    - a hyper-light product is always one kiln holds itself, and so is caught by the rule above;
    - anything else (third-party code only that product vendors, as Turso in Amphora) stays inside
      the product's copy, which is kiln's one copy of it."""
    if not isinstance(upstream, GitUpstream):
        return []
    tool = upstream.show(revision, "scripts/vendor.py")
    if tool is not None and tool != Path(__file__).read_bytes():
        say(f"{project.name} at {revision[:12]} carries a scripts/vendor.py other than kiln's: give it kiln's")
    text = upstream.show(revision, MANIFEST)
    if text is None:
        return []
    try:
        theirs = tomllib.loads(text.decode("utf-8")).get("projects") or {}
    except (tomllib.TOMLDecodeError, UnicodeDecodeError) as e:
        return [f"{project.name} at {revision}: its {MANIFEST} doesn't parse: {e}"]
    out = []
    at = f"{project.name} at {revision[:12]}"
    for name, spec in theirs.items():
        pin = spec.get("commit") or spec.get("version")
        ours = manifest.projects.get(name)
        if ours is None:
            if is_product(spec.get("repository", "")):
                out.append(f"{at} vendors the hyper-light product {name} at {pin}; kiln holds one copy of each product: add {name} to {MANIFEST} at {pin}")
            continue
        if ours.revision != pin:
            out.append(f"{at} takes {name} at {pin}; kiln takes {name} at {ours.revision}: the pins must agree")
        theirs_changes = upstream.files(revision, f"{PATCHES}/{name}")
        ours_changes = {c.name: c.header.encode("utf-8") + c.diff for c in load_changes(manifest.root, ours)}
        if theirs_changes != ours_changes:
            differ = sorted(n for n in set(theirs_changes) | set(ours_changes) if theirs_changes.get(n) != ours_changes.get(n))
            out.append(f"{at} records other changes to {name} than kiln does ({', '.join(differ)}): kiln's must be the product's")
        if takes(project.spec, spec.get("path", f"vendor/{name}")):
            out.append(f"kiln's copy of {project.name} would hold a second {name}: add \"{spec.get('path', f'vendor/{name}')}\" to [projects.{project.name}] exclude")
    return out


def check(manifest: Manifest, names: list[str], sources: dict[str, str], kind: str | None = None) -> int:
    root = manifest.root
    problems: list[str] = []
    for name in names or sorted(manifest.projects):
        project = manifest.projects.get(name)
        if project is None:
            raise Refused(f"no project {name!r} in {MANIFEST}")
        if kind and project.kind != kind:
            continue
        exp = expected_tree(root, project, project.revision, sources.get(name))
        try:
            here = read_vendored(root, project)
            diffs = compare(exp.tree, here)
            problems += [f"{project.path}: {d}" for d in diffs]
            if diffs:
                problems.append(f"{project.path}: record kiln's edits with `scripts/vendor.py record {name} <title> --why <why>`")
            problems += missing_from_notes(root, project, exp.changes)
            problems += pin_problems(manifest, project, project.revision, exp.upstream)
            if isinstance(exp.upstream, BunUpstream) and exp.facts:
                problems += exp.upstream.notice_problems(exp.facts)
            say(f"{name}: {len(exp.tree)} files at {project.revision}, {len(exp.changes)} recorded changes, {len(diffs)} differences")
        finally:
            if exp.scratch:
                exp.scratch.close()
    for p in problems:
        say(p)
    return 1 if problems else 0


def pending_dir(name: str) -> Path:
    """Where a re-vendor of the project waits on a conflict for the person to resolve."""
    return cache_dir() / "conflict" / name


def tree_digest(tree: Tree) -> str:
    h = hashlib.sha256()
    for rel, (mode, data) in sorted(tree.items()):
        h.update(f"{rel}\0{mode}\0{hashlib.sha256(data).hexdigest()}\n".encode("utf-8", errors="surrogateescape"))
    return h.hexdigest()


@dataclasses.dataclass
class Replay:
    """A re-vendor's carrying of the recorded changes onto a new revision, one cherry-pick each in
    the scratch repository, kept beside it when a change conflicts (`--continue`, `--abort`)."""

    project: str
    revision: str
    source: str | None
    here: str  # the digest of kiln's folder when it started: it must not change meanwhile
    prev: str  # the scratch's last commit on the new revision's branch
    plan: list[dict]  # each recorded change in order: {"file", "header", "commit"} (its commit on the old revision)
    at: int = 0  # the next change to carry
    done: list[dict] = dataclasses.field(default_factory=list)  # {"file", "header", "parent", "commit"} on the new branch
    conflicted: list[str] = dataclasses.field(default_factory=list)

    STATE = "kiln-vendor-replay.json"

    def save(self, s: Scratch) -> None:
        (s.dir / ".git" / self.STATE).write_text(json.dumps(dataclasses.asdict(self), indent=1), encoding="utf-8")

    @classmethod
    def load(cls, where: Path) -> "Replay":
        try:
            return cls(**json.loads((where / ".git" / cls.STATE).read_text(encoding="utf-8")))
        except (OSError, ValueError, TypeError) as e:
            raise Refused(f"{where} holds no re-vendor to continue ({e}): remove it with --abort") from None


def carry(root: Path, project: Project, s: Scratch, r: Replay) -> tuple[Tree, list[Change]]:
    """Each recorded change from r.at on, cherry-picked onto the new revision by three-way merge,
    then every change refreshed against it. A change that conflicts stops it: the scratch, git's
    merge in its working tree, is kept as the project's pending re-vendor, and kiln's tree is left
    as it was."""
    root_path = Path(root)
    while r.at < len(r.plan):
        step = r.plan[r.at]
        title = Change(file=Path(step["file"]), header=step["header"], diff=b"").title
        picked = s.git(["cherry-pick", "--no-commit", step["commit"]], ok=True)
        conflicted = s.git(["diff", "--name-only", "--diff-filter=U"]).stdout.decode().split()
        if picked.returncode != 0 or conflicted:
            r.conflicted = conflicted
            where = pending_dir(project.name)
            where.parent.mkdir(parents=True, exist_ok=True)
            s.dir.replace(where)
            s.dir = where
            s.kept = True
            r.save(s)
            files = ", ".join(conflicted) or picked.stderr.decode(errors="replace").strip()
            raise Refused(
                f"{project.name} at {r.revision}: recorded change {step['file']} ({title}) conflicts with upstream in {files}. "
                f"Nothing was changed in {project.path}. git's merge waits in {where}: resolve the files it marks there "
                f"(the change as it should be on upstream's new code), then `scripts/vendor.py {project.name} --continue`; "
                f"or `scripts/vendor.py {project.name} --abort`"
            )
        commit_step(s, r, step, title)
    changes = [
        Change(file=root_path / PATCHES / project.name / d["file"], header=d["header"], diff=s.diff(d["parent"], d["commit"]))
        for d in r.done
    ]
    return s.read(), changes


def commit_step(s: Scratch, r: Replay, step: dict, title: str) -> None:
    """The change under way, as it now stands in the scratch's index, committed; or dropped when
    it changes nothing on the new revision (upstream made it, or its resolution took upstream's)."""
    if s.git(["diff", "--cached", "--quiet"], ok=True).returncode == 0:
        s.git(["cherry-pick", "--quit"], ok=True)
        say(f"{r.project}: {step['file']} ({title}) makes no change on upstream at {r.revision[:12]}")
    else:
        s.git(["commit", "-q", "--no-verify", "-m", title])
        head = s.git(["rev-parse", "HEAD"]).stdout.decode().strip()
        r.done.append({"file": step["file"], "header": step["header"], "parent": r.prev, "commit": head})
        r.prev = head
    r.at += 1


def revendor(manifest: Manifest, name: str, revision: str, source: str | None, gate: bool) -> int:
    root = manifest.root
    project = manifest.projects.get(name)
    if project is None:
        raise Refused(f"no project {name!r} in {MANIFEST}: add its [projects.{name}] first (docs/vendoring.md)")
    if pending_dir(name).exists():
        raise Refused(f"a re-vendor of {name} waits on a conflict in {pending_dir(name)}: resolve it and `--continue`, or `--abort`")
    here = read_vendored(root, project)
    old = None
    if here or load_changes(root, project):
        # The recorded changes, replayed on the revision they were recorded on, to be carried to
        # the new one. And the tree must be what the manifest says before it moves: an edit no
        # change records would be lost.
        old = expected_tree(root, project, project.revision, source)
        diffs = compare(old.tree, here) if here else []
        if diffs:
            if old.scratch:
                old.scratch.close()
            raise Refused(
                f"{project.path} holds edits no recorded change holds; record them first "
                f"(scripts/vendor.py record {name} <title> --why <why>):\n  " + "\n  ".join(diffs)
            )
    try:
        if project.kind == "bun":
            up_b = BunUpstream(project, root)
            new_tree, facts = up_b.tree(revision)
            return finish(manifest, project, revision, here, new_tree, [], facts, up_b, old.facts if old else None, gate)
        up_g = GitUpstream(project, source)
        base = up_g.tree(revision)
        problems = pin_problems(manifest, project, revision, up_g)
        if problems:
            raise Refused("\n  ".join(["the projects' pins don't agree:", *problems]))
        if old is None or old.scratch is None or not old.changes:
            return finish(manifest, project, revision, here, base, [], None, up_g, None, gate)
        s = old.scratch
        s.git(["switch", "-q", "--orphan", "next"])
        s.set_tree(base)
        prev = s.commit(f"{project.name} at {revision}")
        plan = [{"file": c.name, "header": c.header, "commit": commit} for c, commit in zip(old.changes, old.commits)]
        r = Replay(project=name, revision=revision, source=source, here=tree_digest(here), prev=prev, plan=plan)
        new_tree, new_changes = carry(root, project, s, r)
    finally:
        if old and old.scratch and not old.scratch.kept:
            old.scratch.close()
    return finish(manifest, project, revision, here, new_tree, new_changes, None, up_g, None, gate)


def resume(manifest: Manifest, name: str, gate: bool) -> int:
    """`--continue`: the conflicting change as the person resolved it, then the rest carried, and
    kiln's tree written once every change applies."""
    root = manifest.root
    project = manifest.projects.get(name)
    if project is None:
        raise Refused(f"no project {name!r} in {MANIFEST}")
    where = pending_dir(name)
    if not where.exists():
        raise Refused(f"no re-vendor of {name} waits on a conflict")
    r = Replay.load(where)
    here = read_vendored(root, project)
    if tree_digest(here) != r.here:
        raise Refused(f"{project.path} changed since the re-vendor stopped: `scripts/vendor.py {name} --abort`, then start it again")
    s = Scratch.adopt(where)
    s.git(["add", "--all", "--force", "."])
    unmerged = s.git(["diff", "--name-only", "--diff-filter=U"]).stdout.decode().split()
    marked = [f for f in r.conflicted if (where / f).is_file() and re.search(rb"^(<<<<<<< |>>>>>>> )", (where / f).read_bytes(), re.M)]
    if unmerged or marked:
        raise Refused(f"not resolved yet, in {where}: {', '.join(sorted(set(unmerged) | set(marked)))} (git's conflict markers are still there)")
    step = r.plan[r.at]
    commit_step(s, r, step, Change(file=Path(step["file"]), header=step["header"], diff=b"").title)
    r.save(s)
    new_tree, new_changes = carry(root, project, s, r)
    rc = finish(manifest, project, r.revision, here, new_tree, new_changes, None, GitUpstream(project, r.source), None, gate)
    shutil.rmtree(where, ignore_errors=True)
    return rc


def abort(name: str) -> int:
    where = pending_dir(name)
    if not where.exists():
        raise Refused(f"no re-vendor of {name} waits on a conflict")
    shutil.rmtree(where)
    say(f"{name}: the re-vendor that waited on a conflict is gone; nothing was changed")
    return 0


def finish(
    manifest: Manifest,
    project: Project,
    revision: str,
    here: Tree,
    new_tree: Tree,
    new_changes: list[Change],
    facts: BunFacts | None,
    upstream: GitUpstream | BunUpstream,
    old_facts: BunFacts | None,
    gate: bool,
) -> int:
    """Everything is settled: the folder, its recorded changes and the manifest written, then
    the gate."""
    root = manifest.root
    name = project.name
    folder = root / project.path
    folder.mkdir(parents=True, exist_ok=True)
    touched = write_into(folder, here, new_tree)
    patches = root / PATCHES / name
    for c in load_changes(root, project):
        if c.name not in {n.name for n in new_changes}:
            c.file.unlink()
            say(f"{name}: dropped {c.name} ({c.title}): it makes no change on upstream's new tree; remove its row from {project.path}/UPSTREAM.md")
    for c in new_changes:
        data = c.header.encode("utf-8") + c.diff
        if not c.file.exists() or c.file.read_bytes() != data:
            patches.mkdir(parents=True, exist_ok=True)
            c.file.write_bytes(data)
    text = manifest.text
    if project.revision != revision:
        text = set_manifest_value(text, name, project.revision_key, project.revision, revision)
        if isinstance(upstream, BunUpstream) and facts:
            text = set_manifest_value(text, name, "image_digest", project.spec["image_digest"], facts.image_digest)
        notes = folder / "UPSTREAM.md"
        if notes.exists() and project.revision in notes.read_text(encoding="utf-8"):
            notes.write_text(notes.read_text(encoding="utf-8").replace(project.revision, revision), encoding="utf-8")
    if text != manifest.text:
        (root / MANIFEST).write_text(text, encoding="utf-8")
    if isinstance(upstream, BunUpstream) and facts and old_facts:
        upstream.update_notice(old_facts, facts)
        for p in upstream.notice_problems(facts):
            say(f"{p}: update it by hand")
    say(f"{name}: {len(new_tree)} files at {revision}, {len(new_changes)} recorded changes re-applied, {len(touched)} files written or removed")
    # Files kiln's .gitignore would leave out of a commit (tracked ones are never ignored).
    ignored = git(["check-ignore", "--stdin"], cwd=root, input="\n".join(f"{project.path}/{t}" for t in new_tree).encode(), ok=True).stdout.decode().split()
    if ignored:
        say(f"{name}: kiln's .gitignore ignores {len(ignored)} vendored files; add them with git add -f: {', '.join(ignored[:10])}")
    if gate:
        return run_gate(manifest, project)
    say("gate skipped (--no-gate)")
    return 0


def run_gate(manifest: Manifest, project: Project) -> int:
    # kiln's gate first, on the tree as CI builds it; then the project's own commands, which may
    # leave build output behind (Bun's packs the UI server into ui/dist/embed, and a kiln with the
    # UI server packed in unpacks it before it serves, which slows every test server's start).
    for cmd in [*manifest.gate, *project.gate]:
        say(f"gate: {cmd}")
        r = subprocess.run(cmd, shell=True, cwd=manifest.root, check=False)
        if r.returncode != 0:
            say(
                f"gate failed: `{cmd}` exited {r.returncode}. The re-vendored tree is in place: fix it, or put it back "
                f"with `git checkout -- {project.path} {PATCHES}/{project.name} {MANIFEST}`"
            )
            return 1
    say("gate passed")
    return 0


def record(manifest: Manifest, name: str, title: str, why: str) -> int:
    root = manifest.root
    project = manifest.projects.get(name)
    if project is None:
        raise Refused(f"no project {name!r} in {MANIFEST}")
    if project.kind != "git":
        raise Refused(f"{name}: its files are upstream's or kiln's own; it takes no recorded changes")
    if not title.strip() or "\n" in title or not why.strip():
        raise Refused("a change needs a one-line title and a why")
    exp = expected_tree(root, project, project.revision, None)
    try:
        here = read_vendored(root, project)
        if not compare(exp.tree, here):
            raise Refused(f"nothing to record: {project.path} is its upstream plus its recorded changes")
        s = exp.scratch
        if s is None:
            raise Refused(f"{name}: no replay to record against")
        head = s.git(["rev-parse", "HEAD"]).stdout.decode().strip()
        s.set_tree(here)
        s.git(["add", "--all", "--force", "."])
        diff = s.git([*DIFF_ARGS, "--cached", head]).stdout
        folder = root / PATCHES / name
        folder.mkdir(parents=True, exist_ok=True)
        numbers = [int(m.group(1)) for f in folder.glob("*.patch") if (m := re.match(r"(\d+)-", f.name))]
        slug = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")
        if len(slug) > 60:
            slug = slug[:61].rsplit("-", 1)[0]
        slug = slug or "change"
        f = folder / f"{max(numbers, default=0) + 1:04d}-{slug}.patch"
        f.write_bytes(f"Subject: {title.strip()}\nWhy: {' '.join(why.split())}\n\n".encode() + diff)
        say(f"recorded {f.relative_to(root)}; add its row (file, change, why, {f.name}) to {project.path}/UPSTREAM.md")
        return 0
    finally:
        if exp.scratch:
            exp.scratch.close()


def list_projects(manifest: Manifest) -> int:
    for name, p in sorted(manifest.projects.items()):
        where = p.spec.get("repository") or p.spec.get("image")
        n = len(load_changes(manifest.root, p))
        print(f"{name}\t{p.kind}\t{p.revision}\t{p.path}\t{n} recorded changes\t{where}")
    return 0


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="vendor.py", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("command", help="a project's name, or check, record, list")
    ap.add_argument("args", nargs="*")
    ap.add_argument("--from", dest="source", action="append", default=[], help="where to fetch from instead of the manifest's repository: PATH, or PROJECT=PATH for check")
    ap.add_argument("--no-gate", action="store_true", help="don't run the gate after re-vendoring")
    ap.add_argument("--continue", dest="resume", action="store_true", help="carry on a re-vendor that stopped at a conflict, once it's resolved")
    ap.add_argument("--abort", action="store_true", help="drop a re-vendor that stopped at a conflict")
    ap.add_argument("--kind", choices=["git", "bun"], help="check: only the projects of this kind")
    ap.add_argument("--why", default="", help="record: why kiln makes the change")
    ap.add_argument("--root", default=str(Path(__file__).resolve().parent.parent), help=argparse.SUPPRESS)
    a = ap.parse_args(argv)
    try:
        manifest = load_manifest(Path(a.root))
        if a.command == "check":
            sources = dict(s.split("=", 1) for s in a.source if "=" in s)
            return check(manifest, a.args, sources, a.kind)
        if a.command == "record":
            if len(a.args) != 2:
                ap.error("record <project> <title> --why <why>")
            return record(manifest, a.args[0], a.args[1], a.why)
        if a.command == "list":
            return list_projects(manifest)
        if a.resume or a.abort:
            if a.args or (a.resume and a.abort):
                ap.error("<project> --continue | --abort")
            return resume(manifest, a.command, not a.no_gate) if a.resume else abort(a.command)
        if len(a.args) != 1:
            ap.error("<project> <revision>")
        if len(a.source) > 1:
            ap.error("one --from")
        return revendor(manifest, a.command, a.args[0], a.source[0] if a.source else None, not a.no_gate)
    except Refused as e:
        say(str(e))
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
