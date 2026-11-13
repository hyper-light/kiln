#!/usr/bin/env python3
"""Agentfiles kept in kiln, round-tripped against a running server over REST and MCP (kiln plan
01a1cd8e862e7169a074db3d5fa33f3d, step 30): each operation, kept to its owner, and an Agentfile Shards
refuses refused in Shards' own words. tests/agentfiles.rs (`step30_agentfiles_on_every_surface`) runs it
against its own test server; ROOT runs it against production once kiln main is deployed there.

It makes two throwaway people, A and B (grant `developer`, a random suffix to their names), and a
personal access token for each. On each surface A keeps an Agentfile, has an invalid one refused,
reads it, changes it (a new version), lists its versions, restores the first (another version), lists
its own and deletes it; B reaches none of A's. MCP's tools/list must list every agentfiles_* tool.
Each check is one line, `PASS <surface> <check>` or `FAIL <surface> <check>: <why>`; it exits 0 only
when every check passed. Whatever happens, it then deletes the Agentfiles it made, revokes the two
tokens and disables the two people (kiln keeps a disabled person for the record, and never reuses
the name), and says so. No token is ever printed.

    KILN_URL=http://127.0.0.1:<port> KILN_ADMIN_TOKEN=<the server's admin token> python3 live.py

Against production (adacachy), on the person's word, once kiln main runs there:

    ssh adalundhe@192.168.1.77 bash -s <<'EOF'
    set -euo pipefail
    set -a; . /home/adalundhe/.config/kiln/server.env; set +a
    KILN_URL=http://127.0.0.1:7070 python3 <a kiln checkout at main>/docs/research/s30-agentfiles/live.py
    EOF
"""

import json
import os
import secrets
import sys
import urllib.error
import urllib.request

# The texts tests/agentfiles.rs keeps, and the one Shards refuses at its second line.
V1 = 'FROM ubuntu:24.04\nHARNESS claude FROM ./harness\nAGENT main FROM ./agent\nATTACH main FOR claude\nCMD ["claude"]\n'
V2 = 'FROM rust:1.91\nHARNESS claude FROM ./harness\nAGENT main FROM ./agent\nATTACH main FOR claude\nCMD ["claude"]\n'
INVALID = "FROM ubuntu:24.04\nFORM ubuntu\n"
REFUSED = "Shards refuses this Agentfile: dockerfile parse error on line 2: unknown instruction: FORM"
MISSING = "no such Agentfile"
TOOLS = ["agentfiles_list", "agentfiles_get", "agentfiles_create", "agentfiles_update", "agentfiles_versions", "agentfiles_restore", "agentfiles_delete", "agentfiles_check"]


class Failure(Exception):
    pass


def parse(text, what):
    try:
        return json.loads(text)
    except ValueError:
        raise Failure("%s answered what isn't JSON: %s" % (what, text[:200])) from None


class Kiln:
    """One server, and every secret it must never print."""

    def __init__(self, url, admin):
        self.url = url.rstrip("/")
        self.admin = admin
        self.secrets = [admin]
        self.ids = 0

    def hide(self, text):
        for s in self.secrets:
            if s:
                text = text.replace(s, "<token>")
        return text

    def http(self, method, path, token, body=None):
        """(status, text) of one request; a server that can't be reached is a Failure."""
        data = None if body is None else json.dumps(body).encode()
        headers = {"Authorization": "Bearer " + token, "Accept": "application/json"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(self.url + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req) as r:
                return r.status, r.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode("utf-8", "replace")
        except (urllib.error.URLError, OSError) as e:
            raise Failure("%s %s: %s" % (method, path, e)) from None

    def rpc(self, token, method, params):
        """One JSON-RPC request to /api/mcp: its result, or a Failure for a protocol error."""
        self.ids += 1
        status, text = self.http("POST", "/api/mcp", token, {"jsonrpc": "2.0", "id": self.ids, "method": method, "params": params})
        if status != 200:
            raise Failure("/api/mcp answered %d: %s" % (status, self.hide(text)[:300]))
        answer = parse(text, "/api/mcp " + method)
        if "error" in answer:
            raise Failure("%s: protocol error %s" % (method, json.dumps(answer["error"])))
        return answer.get("result", {})


class Rest:
    name = "REST"

    def __init__(self, kiln):
        self.k = kiln

    def _do(self, method, path, token, body=None):
        status, text = self.k.http(method, path, token, body)
        if 200 <= status < 300:
            return True, parse(text, "%s %s" % (method, path)) if text else None, status
        return False, text, status

    def list(self, t):
        return self._do("GET", "/api/agentfiles", t)

    def get(self, t, id):
        return self._do("GET", "/api/agentfiles/" + id, t)

    def create(self, t, name, text):
        return self._do("POST", "/api/agentfiles", t, {"name": name, "text": text})

    def update(self, t, id, text):
        return self._do("PATCH", "/api/agentfiles/" + id, t, {"text": text})

    def versions(self, t, id):
        return self._do("GET", "/api/agentfiles/%s/versions" % id, t)

    def restore(self, t, id, version):
        return self._do("POST", "/api/agentfiles/%s/restore" % id, t, {"version": version})

    def delete(self, t, id):
        return self._do("DELETE", "/api/agentfiles/" + id, t)

    def check(self, t, text):
        return self._do("POST", "/api/agentfiles/check", t, {"text": text})


class Mcp:
    name = "MCP"

    def __init__(self, kiln):
        self.k = kiln

    def _do(self, t, tool, arguments):
        """A tool call: (ok, its result's JSON, None), or (False, the error's words, None)."""
        r = self.k.rpc(t, "tools/call", {"name": tool, "arguments": arguments})
        content = r.get("content") or [{}]
        text = content[0].get("text", "")
        if r.get("isError", True):
            return False, text, None
        return True, parse(text, tool), None

    def list(self, t):
        return self._do(t, "agentfiles_list", {})

    def get(self, t, id):
        return self._do(t, "agentfiles_get", {"id": id})

    def create(self, t, name, text):
        return self._do(t, "agentfiles_create", {"name": name, "text": text})

    def update(self, t, id, text):
        return self._do(t, "agentfiles_update", {"id": id, "text": text})

    def versions(self, t, id):
        return self._do(t, "agentfiles_versions", {"id": id})

    def restore(self, t, id, version):
        return self._do(t, "agentfiles_restore", {"id": id, "version": version})

    def delete(self, t, id):
        return self._do(t, "agentfiles_delete", {"id": id})

    def check(self, t, text):
        return self._do(t, "agentfiles_check", {"text": text})


class Run:
    def __init__(self, kiln):
        self.k = kiln
        self.failed = 0
        self.people = []  # (id, name)
        self.tokens = []  # ids
        self.agentfiles = []  # ids

    def say(self, surface, check, ok, why=""):
        if ok:
            print("PASS %s %s" % (surface, check), flush=True)
        else:
            self.failed += 1
            print("FAIL %s %s: %s" % (surface, check, self.k.hide(str(why))[:400]), flush=True)

    def person(self, name):
        """A throwaway person with grant `developer`, and a token that does all they may."""
        status, text = self.k.http("POST", "/api/users", self.k.admin, {"name": name, "grants": ["developer"]})
        if status not in (200, 201):
            raise Failure("POST /api/users %s: %d %s" % (name, status, self.k.hide(text)[:300]))
        self.people.append((parse(text, "POST /api/users")["id"], name))
        status, text = self.k.http("POST", "/api/tokens", self.k.admin, {"kind": "pat", "user": name, "grants": ["owner"]})
        if status not in (200, 201):
            raise Failure("POST /api/tokens for %s: %d %s" % (name, status, self.k.hide(text)[:300]))
        made = parse(text, "POST /api/tokens")
        self.tokens.append(made["id"])
        self.k.secrets.append(made["token"])
        return made["token"]

    def surface(self, s, a, b, suffix):
        name = "s30-%s-%s" % (s.name.lower(), suffix)
        ok, v, _ = s.create(a, name, V1)
        agentfile = v.get("agentfile", {}) if ok and isinstance(v, dict) else {}
        id = agentfile.get("id")
        if id:
            self.agentfiles.append(id)
        self.say(s.name, "create keeps version 1, its owner's", ok and agentfile.get("version") == 1 and str(agentfile.get("owner", "")).startswith("user:") and "check" in v, v)
        if not id:
            raise Failure("no Agentfile was kept to go on with")

        ok, said, status = s.create(a, name + "-invalid", INVALID)
        self.say(s.name, "an invalid Agentfile is refused in Shards' words", not ok and REFUSED in str(said) and status in (None, 422), "%s %s" % (status, said))
        ok, v, _ = s.check(a, INVALID)
        first = (v.get("diagnostics") or [{}])[0] if ok and isinstance(v, dict) else {}
        self.say(s.name, "check finds Shards' error at line 2, keeping nothing", ok and first.get("severity") == "error" and first.get("range", {}).get("start", {}).get("line") == 2, v)

        ok, v, _ = s.get(a, id)
        self.say(s.name, "get reads its text and version", ok and v.get("text") == V1 and v.get("version") == 1, v)
        ok, v, _ = s.update(a, id, V2)
        self.say(s.name, "update with a new text is version 2", ok and v.get("agentfile", {}).get("version") == 2 and v.get("agentfile", {}).get("text") == V2, v)
        ok, v, _ = s.versions(a, id)
        listed = [x.get("version") for x in v.get("versions", [])] if ok else v
        self.say(s.name, "versions come newest first", ok and listed == [2, 1], listed)
        ok, v, _ = s.restore(a, id, 1)
        self.say(s.name, "restore makes version 1's text version 3", ok and v.get("agentfile", {}).get("version") == 3 and v.get("agentfile", {}).get("text") == V1, v)
        ok, v, _ = s.list(a)
        names = [x.get("name") for x in v.get("agentfiles", [])] if ok else v
        self.say(s.name, "list holds the owner's own, and not the refused one", ok and name in names and name + "-invalid" not in names and v.get("reaches_all") is False, names)

        for check, call in [
            ("get", lambda: s.get(b, id)),
            ("versions", lambda: s.versions(b, id)),
            ("update", lambda: s.update(b, id, V2)),
            ("restore", lambda: s.restore(b, id, 1)),
            ("delete", lambda: s.delete(b, id)),
        ]:
            ok, said, status = call()
            self.say(s.name, "another person's %s finds no such Agentfile" % check, not ok and MISSING in str(said) and status in (None, 404), "%s %s" % (status, said))
        ok, v, _ = s.list(b)
        theirs = [x.get("id") for x in v.get("agentfiles", [])] if ok else v
        self.say(s.name, "another person's list holds none of the owner's", ok and id not in theirs, theirs)

        ok, v, _ = s.delete(a, id)
        self.say(s.name, "delete takes it and its versions", ok and v == {"deleted": True}, v)
        ok, said, status = s.get(a, id)
        self.say(s.name, "after a delete, get finds no such Agentfile", not ok and MISSING in str(said) and status in (None, 404), "%s %s" % (status, said))

    def tools(self, a):
        listed = [t.get("name") for t in self.k.rpc(a, "tools/list", {}).get("tools", [])]
        missing = [t for t in TOOLS if t not in listed]
        self.say("MCP", "tools/list lists every agentfiles_* tool", not missing, "missing %s" % missing)

    def initialize(self, a):
        r = self.k.rpc(a, "initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "s30-live", "version": "1"}})
        self.say("MCP", "initialize answers as kiln", r.get("serverInfo", {}).get("name") == "kiln", r.get("serverInfo"))

    def clean(self):
        """Every Agentfile it made, the two tokens and the two people, taken back with the admin token."""
        gone = already = 0
        for id in self.agentfiles:
            status, text = self.k.http("DELETE", "/api/agentfiles/" + id, self.k.admin)
            if 200 <= status < 300:
                gone += 1
            elif status == 404:
                already += 1
            else:
                self.say("cleanup", "delete Agentfile %s" % id, False, "%d %s" % (status, text))
        for id in self.tokens:
            status, text = self.k.http("DELETE", "/api/tokens/" + id, self.k.admin)
            if not 200 <= status < 300:
                self.say("cleanup", "revoke token %s" % id, False, "%d %s" % (status, text))
        for id, name in self.people:
            status, text = self.k.http("DELETE", "/api/users/" + id, self.k.admin)
            if not 200 <= status < 300:
                self.say("cleanup", "disable %s" % name, False, "%d %s" % (status, text))
        print(
            "cleanup: %d Agentfile(s) deleted (%d gone already), %d token(s) revoked, %d person(s) disabled: %s"
            % (gone, already, len(self.tokens), len(self.people), ", ".join(n for _, n in self.people) or "none made"),
            flush=True,
        )


def main():
    url, admin = os.environ.get("KILN_URL", ""), os.environ.get("KILN_ADMIN_TOKEN", "")
    if not url or not admin:
        print("set KILN_URL (http://127.0.0.1:7070) and KILN_ADMIN_TOKEN (the server's admin token)", file=sys.stderr)
        return 2
    kiln = Kiln(url, admin)
    run = Run(kiln)
    suffix = secrets.token_hex(4)
    try:
        a = run.person("s30-live-a-" + suffix)
        b = run.person("s30-live-b-" + suffix)
        run.initialize(a)
        run.tools(a)
        for s in (Rest(kiln), Mcp(kiln)):
            try:
                run.surface(s, a, b, suffix)
            except Failure as e:
                run.say(s.name, "round trip", False, e)
            except Exception as e:  # an answer of another shape than kiln's: said, and the rest goes on
                run.say(s.name, "round trip", False, "%s: %s" % (type(e).__name__, e))
    except Failure as e:
        run.say("setup", "people and tokens", False, e)
    except Exception as e:
        run.say("setup", "people and tokens", False, "%s: %s" % (type(e).__name__, e))
    finally:
        try:
            run.clean()
        except Failure as e:
            run.say("cleanup", "reach the server", False, e)
    print("%s: %d check(s) failed" % ("FAILED" if run.failed else "PASSED", run.failed), flush=True)
    return 1 if run.failed else 0


if __name__ == "__main__":
    sys.exit(main())
