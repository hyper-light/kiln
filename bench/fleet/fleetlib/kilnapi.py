"""The kiln driver: harness microVMs started as kiln's own isolated agent sessions, placed by kiln's
balancer among the runners that start microVMs (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d steps 17
and 32).

An ask is `agents.start` over kiln's API (`POST /api/harness/agents`): one agent of the harness
`--kind`, `isolation: microvm` (a microVM of its own), `on: microvm` (any machine whose runner says it
starts them: the balancer places it, by the runners' own admission), named by its token. kiln's
answer is the agent (its id and machine), the word that it waits for room (`queued`), or a refusal
in kiln's words (an HTTP status and its text: `no machine suits ...`). `agents.get` follows it until
kiln says it stands ready (idle, working or waiting on a person), ended or failed; `agents.list` is
the view; `agents.stop` removes it.

Its VMs run on the runners kiln picks, not here: the agent scans no process of theirs (`scans`
False), and the witnesses judge kiln's own view (a live ask kiln no longer holds is `lost`, one it
holds for an ended ask `unasked`). What the runners' hosts show (a VM no agent holds) is each host's
own scan's, owed (README, Owed). The session gives the harness no byte lane: the hold's round trips
and streams are owed too.

kiln's address and a token are the environment's (`KILN_URL`, `KILN_TOKEN`): the token is sent as
a bearer token and never recorded.
"""

from __future__ import annotations

import hashlib
import json
import os
import time
import urllib.error
import urllib.request
from typing import Dict, List, Optional, Set, Tuple

# kiln's stands (agents.list): an agent ready for its next message, at work, or waiting on a person
# stands made; one ended, stopped or gone stands ended.
READY = {"idle", "working", "needs_you"}
GONE = {"ended", "stopped"}


class Kiln:
    lanes = False
    actions: Tuple[str, ...] = ()
    name = "kiln"
    scans = False

    def __init__(self, url: str, token: str, kind: str, shards: str, scope: str) -> None:
        self.url = url.rstrip("/")
        self.token = token
        self.kind = kind
        self.shards_binary = shards
        self.scope = scope
        self.mark = scope
        self.asked: Set[str] = set()
        self.ids: Dict[str, str] = {}
        self.machines: Dict[str, str] = {}
        self.ended: Set[str] = set()

    def call(self, method: str, path: str, body: Optional[dict] = None) -> Tuple[int, object]:
        """One request to kiln's API: its HTTP status and its JSON answer (or its text)."""
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.url + path, data=data, method=method, headers={"Authorization": f"Bearer {self.token}", "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req) as r:
                status, raw = r.status, r.read()
        except urllib.error.HTTPError as e:
            status, raw = e.code, e.read()
        except (urllib.error.URLError, OSError) as e:
            return 0, f"kiln unreachable: {e}"
        text = raw.decode("utf-8", "replace")
        try:
            return status, json.loads(text) if text.strip() else None
        except ValueError:
            return status, text

    # ---------------------------------------------------------------- the placer itself

    def start(self, log_path: str, deadline_ns: Optional[int]) -> Tuple[bool, str]:
        """kiln is already serving: its answer to a list of agents is its readiness."""
        status, answer = self.call("GET", "/api/harness/agents")
        if status != 200:
            return False, f"kiln answered {status}: {str(answer)[:300]}"
        return True, ""

    def stop(self) -> Optional[int]:
        return None

    def roots(self) -> List[int]:
        return []

    def placer(self) -> Dict[str, object]:
        return {"url": self.url}

    def identity(self) -> Dict[str, str]:
        """The shards binary's SHA-256 where one is named (kiln carries its own once step 16
        lands), and kiln's own version as it says it."""
        out: Dict[str, str] = {}
        if self.shards_binary and os.path.isfile(self.shards_binary):
            h = hashlib.sha256()
            with open(self.shards_binary, "rb") as f:
                for chunk in iter(lambda: f.read(1 << 20), b""):
                    h.update(chunk)
            out["shards"] = h.hexdigest()
        return out

    def version(self) -> str:
        status, answer = self.call("GET", "/api/version")
        return json.dumps(answer)[:200] if status == 200 else f"kiln answered {status}"

    def pull(self, image: str) -> Tuple[bool, str]:
        return True, "kiln's runners hold their harness images"

    def run_once(self, image: str, deadline_ns: Optional[int]) -> Tuple[bool, str]:
        """One session started, followed until kiln says it stands ready, and stopped: the first
        start's costs paid once (the runner's image, its VM's template), and a start's time to seed
        the start wait with. Followed at doubling intervals from 1 ms (kiln gives no event here),
        bounded by the run's deadline alone, as setup's first waits are."""
        token = "first-run"
        why = self.place(token, image, 0)
        try:
            if why is not None:
                return False, why
            wait = 0.001
            while True:
                state = self.placed(token)
                if state == "live":
                    return True, f"ready on {self.machines.get(token, '')}"
                if state != "starting":
                    return False, state
                if deadline_ns is not None and time.monotonic_ns() > deadline_ns:
                    return False, "the first session wasn't ready within the run's deadline"
                time.sleep(wait)
                wait *= 2
        finally:
            self.remove([token])
            for held in (self.asked, self.ended):
                held.discard(token)
            self.ids.pop(token, None)

    # ---------------------------------------------------------------- asks

    def place(self, token: str, image: str, ballast: int) -> Optional[str]:
        """Asks kiln for the ask's agent; None once kiln started it, else why not in kiln's words.
        One kiln holds for room (`queued`) is said so: the fleet is full by kiln's own admission,
        and the drain still stops it should it start."""
        self.asked.add(token)
        status, answer = self.call("POST", "/api/harness/agents", {"kind": self.kind, "on": "microvm", "isolation": "microvm", "name": token, "count": 1})
        if status != 200 or not isinstance(answer, dict):
            return f"kiln answered {status}: {str(answer)[:300]}"
        agents = answer.get("agents") or []
        if agents and isinstance(agents[0], dict) and agents[0].get("id"):
            self.ids[token] = str(agents[0]["id"])
            self.machines[token] = str(agents[0].get("machine") or "")
            return None
        if answer.get("queued"):
            return f"queued: {str(answer['queued'])[:300]}"
        return f"kiln started no agent: {json.dumps(answer)[:300]}"

    def find(self, token: str) -> Optional[dict]:
        """The ask's agent as kiln lists it (a queued ask's agent appears once it starts)."""
        status, answer = self.call("GET", "/api/harness/agents")
        rows = answer.get("agents") if isinstance(answer, dict) else answer
        for row in rows if isinstance(rows, list) else []:
            if isinstance(row, dict) and row.get("name") == token:
                self.ids[token] = str(row.get("id"))
                self.machines[token] = str(row.get("machine") or "")
                return row
        return None

    def placed(self, token: str) -> str:
        agent_id = self.ids.get(token)
        if agent_id is None:
            row = self.find(token)
            return "starting" if row is None else stand_of(row)
        status, answer = self.call("GET", f"/api/harness/agents/{agent_id}")
        if status == 404:
            return "ended"
        if status != 200 or not isinstance(answer, dict):
            return f"refused: kiln answered {status}: {str(answer)[:200]}"
        self.machines[token] = str(answer.get("machine") or self.machines.get(token, ""))
        return stand_of(answer)

    def where(self, token: str) -> str:
        return self.machines.get(token, "")

    def view(self) -> Tuple[Dict[str, Optional[List[int]]], str]:
        status, answer = self.call("GET", "/api/harness/agents")
        if status != 200:
            return {}, f"kiln answered {status}"
        rows = answer.get("agents") if isinstance(answer, dict) else answer
        view: Dict[str, Optional[List[int]]] = {}
        for row in rows if isinstance(rows, list) else []:
            if not isinstance(row, dict) or str(row.get("name")) not in self.asked:
                continue
            token = str(row.get("name"))
            if stand_of(row) == "live":
                view[token] = None
            elif stand_of(row) == "ended":
                self.ended.add(token)
        return view, ""

    def remove(self, tokens: List[str]) -> Tuple[int, str]:
        worst, said = 0, []
        for token in tokens:
            if token not in self.ids and token in self.asked:
                self.find(token)
            agent_id = self.ids.get(token)
            if agent_id is None:
                continue
            status, answer = self.call("POST", f"/api/harness/agents/{agent_id}/stop", {"agent": agent_id})
            if status not in (200, 204, 404):
                worst = status or 1
                said.append(f"{token}: {status} {str(answer)[:120]}")
        return worst, "; ".join(said)[-500:]

    def scan(self, table, roots: List[int]) -> List[Dict[str, object]]:
        return []


def stand_of(row: dict) -> str:
    stand = str(row.get("stand") or row.get("status") or "")
    if stand in READY:
        return "live"
    if stand in GONE:
        return "ended"
    if stand == "failed":
        return f"refused: kiln says it failed: {str(row.get('error') or row.get('why') or '')[:200]}"
    return "starting"
