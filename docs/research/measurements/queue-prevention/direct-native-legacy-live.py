#!/usr/bin/env python3
"""Root-only disposable legacy f555 -> exact verified Unix candidate upgrade proof; PREP ONLY.

Spec.prompt creates one actual legacy input with no server publication/token. Require
its real UUID/body/PID, post-adoption legacy_opaque debt, actual epoch-1 certificate,
provider-child agents.result with both identity marks removed, and real completion
returning receiver debt. Root executes with normal admission and reviews cleanup.
No Windows/crash/multi-provider/production safety claim. Baseline is a preserved deployed
artifact, never a fabricated new unchanged-source build receipt.
POLICY_READ implementer0d261aa931ae2e33e1f4a528b33f7e26b5ee7c90; rules SHA256
 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from urllib.parse import unquote, urlsplit


class Refused(Exception):
    pass


def require(condition, why):
    if not condition:
        raise Refused(why)


def sha(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def private(path, text):
    with open(path, "x", encoding="utf-8") as stream:
        os.chmod(path, 0o600)
        stream.write(text)


def port(kind):
    with socket.socket(socket.AF_INET, kind) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def rows(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def agent_identity(subject):
    """Parse the public Actor::Agent URI shape in kiln-ci identity/chain.rs."""
    if not isinstance(subject, str):
        return None
    uri = urlsplit(subject)
    parts = [unquote(part) for part in uri.path.strip("/").split("/")]
    if (uri.scheme != "kiln" or not uri.netloc or uri.query or uri.fragment or len(parts) != 10
            or parts[0] != "person" or parts[2] != "agent" or parts[4] != "epoch"
            or parts[6] != "machine" or parts[8] != "instance" or not parts[5].isascii()
            or not parts[5].isdigit() or str(int(parts[5])) != parts[5]):
        return None
    return {"session": parts[3], "epoch": int(parts[5]), "machine": parts[7], "instance": parts[9]}


def identity_log_observations(text, session, allowed_epochs):
    """Public URI and runner identity::slot formats; retain no raw private log lines."""
    slot = re.compile(r"\bagent/" + re.escape(session) + r"/([0-9]+)\b")
    uri = re.compile(r"/agent/" + re.escape(session) + r"/epoch/([0-9]+)(?=/|\b)")
    identity_field = re.compile(r'\bidentity=(?:"([^"]+)"|([^\s]+))')
    observed, refused = set(), set()
    for line in text.splitlines():
        line = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", line)
        epochs = {int(value) for pattern in (slot, uri) for value in pattern.findall(line)}
        observed.update(epochs)
        if "kiln issued no identity" in line or "no identity issued" in line:
            # The reason can reference a valid prior hand-on URI; only identity= was refused.
            for quoted, bare in identity_field.findall(line):
                value = quoted or bare
                if match := slot.fullmatch(value):
                    refused.add(int(match[1]))
                elif value.startswith("kiln://"):
                    refused.update(int(epoch) for epoch in uri.findall(value))
    return {"observed_epochs": sorted(observed), "unexpected_epochs": sorted(observed - set(allowed_epochs)),
            "refused_epochs": sorted(refused), "destination_refused": max(allowed_epochs) in refused}


def verify_identity_log_parser():
    line = ("WARN kiln issued no identity identity=agent/S/2 error=hand-on "
            "kiln://fixture/person/admin/agent/S/epoch/1/machine/a/instance/old not to "
            "kiln://fixture/person/admin/agent/S/epoch/2/machine/a/instance/new")
    facts = identity_log_observations(line, "S", (0, 1))
    require(facts["refused_epochs"] == [2] and facts["unexpected_epochs"] == [2]
            and facts["destination_refused"] is False, "identity parser confused a referenced hand-on with a refused request")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--source-receipt", required=True, type=Path)
    parser.add_argument("--candidate-source", required=True, help="Exact signed source commit in the actual candidate build receipt")
    parser.add_argument("--before-binary", required=True, type=Path)
    parser.add_argument("--before-sha256", required=True)
    parser.add_argument("--before-linux-copy", type=Path, help="Root-verified immutable Linux f555 copy; exact preserved hash remains required")
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path, help="New private directory on disk, preferably under ~/.cache")
    parser.add_argument("--timeout-seconds", required=True, type=float)
    parser.add_argument("--keep-free-bytes", required=True, type=int, help="Coordinator's effective operational floor")
    args = parser.parse_args()
    verify_identity_log_parser()
    require(os.name == "posix", "this direct SCM proof is Unix-only")
    require(args.timeout_seconds > 0 and args.keep_free_bytes >= 0, "invalid fixture envelope")
    binary = args.binary.resolve(strict=True)
    require(sha(binary) == args.sha256, "binary does not match the coordinator's expected SHA256")
    require(args.source_receipt.is_file(), "the build/source receipt is absent")
    prior = args.before_binary.resolve(strict=True)
    host = (sys.platform, os.uname().machine)
    if host == ("darwin", "arm64"):
        require(args.before_linux_copy is None and str(prior) == "/private/tmp/kiln-offline-fix-20261110/dist/kiln-aarch64-apple-darwin",
                "Mac baseline must be the preserved f555 artifact; never the installed canonical file")
        baseline_sha256 = "be797cf09ec95a61f0cb95dcb3d0714bbc6be258182a2a2f09f5f90ef1685a84"
    elif host == ("linux", "x86_64"):
        immutable = args.before_linux_copy.resolve(strict=True) if args.before_linux_copy is not None else None
        require(str(prior) == "/home/adalundhe/.local/share/kiln-server/bin/kiln" or prior == immutable,
                "Linux baseline must be the exact preserved service artifact or explicit Root-verified immutable copy")
        baseline_sha256 = "87107f09976eb189264a7d1e5ee7fc7435797507a819be51ff89b4234044c8ff"
    else:
        raise Refused("no preserved f555 baseline pinned for this platform/architecture")
    require(args.before_sha256 == baseline_sha256 and sha(prior) == baseline_sha256,
            "preserved deployed f555 baseline hash differs")
    require(re.fullmatch("[0-9a-f]{40}", args.candidate_source) is not None, "candidate source must be an exact signed commit")
    source = json.loads(args.source_receipt.read_bytes())
    require(source.get("exit_code") == 0 and source.get("source_unchanged") is True and source.get("sha256") == args.sha256
            and source.get("source_commit") == args.candidate_source,
            "candidate lacks Root's successful unchanged-source build receipt")
    root = args.out.absolute()
    root.parent.mkdir(parents=True, exist_ok=True)
    require(shutil.disk_usage(root.parent).free >= args.keep_free_bytes + binary.stat().st_size * 2,
            "fixture disk copies would cross the coordinator's operational floor")
    root.mkdir(mode=0o700)
    started = time.monotonic()
    deadline = started + args.timeout_seconds
    children = []
    runtime = Path(tempfile.mkdtemp(prefix="kn-", dir="/tmp" if sys.platform == "linux" else "/private/tmp"))
    providers = {}
    admin = None
    session = None
    destination_epoch = None
    live_complete = False
    http_port = port(socket.SOCK_STREAM)
    quic_port = port(socket.SOCK_DGRAM)
    report = {"binary_sha256": args.sha256, "source_receipt_sha256": sha(args.source_receipt),
              "source_receipt": str(args.source_receipt.resolve()), "out": str(root),
              "kind": "actual-isolated-legacy-f555-native-scm", "checks": {}, "passed": False, "runtime": str(runtime),
              "baseline": {"source": "f555736bd887a4196c8841564cb9b4d6c1ede95f", "path": str(prior), "sha256": args.before_sha256, "provenance": "preserved deployed artifact; no new unchanged-source receipt claimed"},
              "expected_candidate_source": args.candidate_source}

    def check(name, condition):
        require(condition, "failed witness: " + name)
        report["checks"][name] = True

    def api(method, path, body=None, allowed=(200, 202)):
        connection = http.client.HTTPConnection("127.0.0.1", http_port, timeout=min(75, max(1, deadline - time.monotonic())))
        encoded = None if body is None else json.dumps(body).encode()
        headers = {"Content-Type": "application/json"}
        if admin:
            headers["Authorization"] = "Bearer " + admin
        try:
            connection.request(method, path, encoded, headers)
            response = connection.getresponse()
            data = response.read()
            require(response.status in allowed, "fixture HTTP operation refused: " + str(response.status) + " " + path)
            if not data:
                return None
            try:
                return json.loads(data)
            except ValueError:
                return data.decode("utf-8", errors="replace")
        finally:
            connection.close()

    def wait(name, observe):
        while time.monotonic() < deadline:
            for role, child in children:
                require(child.poll() is None, "owned " + role + " exited before the witness")
            try:
                found = observe()
                if found:
                    return found
            except (ConnectionError, TimeoutError, OSError, sqlite3.OperationalError):
                pass
            time.sleep(0.1)  # Bounded test observation, not a product retry/probe.
        raise Refused("fixture deadline expired: " + name)

    def launch(role, argv, environment):
        log = open(root / (role + ".log"), "xb")
        os.chmod(log.name, 0o600)
        try:
            child = subprocess.Popen(argv, env=environment, stdin=subprocess.DEVNULL,
                                     stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        finally:
            log.close()
        children.append((role, child))
        private(root / (role + ".pid.json"), json.dumps({"pid": child.pid, "role": role}) + "\n")
        return child

    def source_info():
        return api("GET", "/api/harness/sessions/" + session)

    def initial_identity():
        # Info and State omit epoch0 on the wire. The certified subject names it explicitly.
        value = api("GET", "/api/identities?kind=agent")
        require(isinstance(value, dict) and isinstance(value.get("identities"), list), "public identities response shape changed")
        matches = []
        for certificate in value["identities"]:
            parsed = agent_identity(certificate.get("subject"))
            if not parsed or parsed["session"] != session:
                continue
            require(parsed["epoch"] == 0 and parsed["machine"] == machine, "fresh legacy certified identity has an unexpected epoch or owner")
            if certificate.get("refused") is None and isinstance(certificate.get("verified"), str) and certificate["verified"]:
                matches.append({"epoch": parsed["epoch"], "certificate": {key: certificate[key] for key in ("serial", "kind", "subject", "key", "verified", "refused")}})
        require(len(matches) <= 1, "multiple verified source identities")
        return matches[0] if matches else None

    def legacy_pending():
        # Called only after the owned writer group has exited. Turso holds an exclusive
        # lock while alive, so live SQLite reads are not a valid observation seam.
        parsed = agent_identity(report.get("destination_certificate", {}).get("subject"))
        require(parsed is not None and parsed["session"] == session and parsed["epoch"] == destination_epoch,
                "no exact destination identity for its persisted records")
        state = root / "runner-state"
        owner = state if parsed["instance"] in ("main", "legacy") else state / "instances" / parsed["instance"]
        path = owner / "records.db"
        require(path.resolve().is_relative_to(state.resolve()), "destination records path escapes owned fixture")
        report["typed_pending_path"] = str(path)
        connection = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=1)
        try:
            row = connection.execute("SELECT value FROM versions WHERE family=? AND key=? ORDER BY rev DESC LIMIT 1",
                                     ("inbox-pending", (session + "/" + message).encode())).fetchone()
        finally:
            connection.close()
        require(row is not None and row[0] is not None, "exact persisted legacy Pending is absent, not empty proof")
        return {"path": str(path), "pending": json.loads(row[0])}

    def held_debt():
        info = source_info()
        report["last_legacy_info"] = {"epoch": info.get("epoch"), "epoch_present": "epoch" in info,
                                      "status": info.get("status"), "admission": info.get("admission")}
        capacity = info.get("admission")
        if not isinstance(capacity, dict) or capacity.get("epoch") != destination_epoch or capacity.get("admitted") != 1 or capacity.get("bytes", 0) <= 0:
            return None
        return {"capacity": capacity, "observation": "public receiver debt while original native result gate is held; typed row deferred until writer closes"}

    def process_identity(pid):
        result = subprocess.run(["ps", "-ww", "-p", str(pid), "-o", "lstart=", "-o", "command="], capture_output=True, text=True, env={**os.environ, "LC_ALL": "C"}, timeout=5)
        text = result.stdout.strip()
        return text if result.returncode == 0 and str(root / "bin" / "claude") in text else None

    def identity_logs(expected_epoch):
        raw = (root / "supervisor.log").read_bytes()
        facts = identity_log_observations(raw.decode("utf-8", errors="replace"), session, (initial["epoch"], expected_epoch))
        report["identity_log"] = {**facts, "sha256": hashlib.sha256(raw).hexdigest()}
        require(not facts["unexpected_epochs"] and not facts["destination_refused"],
                "owned supervisor requested or refused an unexpected destination identity epoch; inspect private log")
        return facts

    def destination_certificate():
        identity_logs(destination_epoch)
        value = api("GET", "/api/identities?kind=agent")
        require(isinstance(value, dict) and isinstance(value.get("identities"), list), "public identities response shape changed")
        matches = []
        for certificate in value["identities"]:
            subject = certificate.get("subject")
            parsed = agent_identity(subject)
            if isinstance(subject, str) and "/agent/" + session + "/" in subject:
                require(parsed is not None, "owned agent certificate has an unreadable subject")
            if not parsed or parsed["session"] != session:
                continue
            require(parsed["epoch"] in (initial["epoch"], destination_epoch), "public agent identity has an unexpected epoch")
            if parsed["epoch"] == destination_epoch:
                require(parsed["machine"] == machine and bool(parsed["instance"]), "destination certificate names a different owner")
                if certificate.get("refused") is None and isinstance(certificate.get("verified"), str) and certificate["verified"]:
                    matches.append(certificate)
        require(len(matches) <= 1, "multiple verified destination identities")
        return matches[0] if matches else None

    def interrupted(_signal, _frame):
        raise Refused("the coordinator interrupted this owned fixture")
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)

    try:
        for name in ("home", "bin", "work-here", "runner-state", "runner-work"):
            (root / name).mkdir(mode=0o700)
        helper = args.repo / "crates/kiln/tests/fixtures/fake-claude/claude"
        helper_text = helper.read_text()
        original = 'out({"type": "result", "subtype": "success", "is_error": False, "duration_ms": 1, "num_turns": 1, "result": "done", "session_id": session, "total_cost_usd": 0, "usage": {"input_tokens": 1, "output_tokens": 1}})'
        replacement = original.replace('"duration_ms": 1,', '"duration_ms": 1, "duration_api_ms": 1, "terminal_reason": "completed", "user_message_uuid": command,')
        require(helper_text.count(original) == 1, "fake-claude completion seam changed; review the fixture instead of silently patching")
        # The copied stand-in reports the exact original ID at its actual gated completion.
        # Started alone never counts as consumption in the production adapter.
        replacement += '\n        with open(".fake-claude-completed", "a") as completed:\n            completed.write(json.dumps({"uuid": command, "pid": os.getpid(), "session": session}) + "\\n")'
        private(root / "bin" / "claude", helper_text.replace(original, replacement))
        os.chmod(root / "bin" / "claude", 0o700)
        report["helper_original_sha256"] = sha(helper)
        report["helper_fixture_sha256"] = sha(root / "bin" / "claude")
        before = root / "kiln-before"
        with open(prior, "rb") as source, open(before, "xb") as destination:
            shutil.copyfileobj(source, destination, 1024 * 1024)
        os.chmod(before, 0o700)
        report["before_sha256"] = sha(before)
        require(report["before_sha256"] != args.sha256, "the before/after artifacts must differ to exercise upgrade")
        require(report["before_sha256"] == args.before_sha256, "private baseline copy changed")
        config = f'''[server]
http_addr = "127.0.0.1:{http_port}"
quic_addr = "127.0.0.1:{quic_port}"
public_url = "http://127.0.0.1:{http_port}"
quic_advertise = "127.0.0.1:{quic_port}"
data_dir = {json.dumps(str(root / "data"))}
https = false
[gitops]
auto_pipelines = false
[local_agent]
enabled = false
[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false
[cd]
enabled = false
[terminal]
enabled = false
[kubernetes]
enabled = false
[ci]
enabled = false
signatures = "warn"
[ci.scheduler]
headroom = false
'''
        private(root / "kiln.toml", config)
        # Child processes inherit no production endpoint, supervisor socket, credential,
        # signing configuration, or model account. PATH supplies only programs; the selected
        # harness program itself is the absolute owned fake path above.
        env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "TZ", "TMPDIR") if key in os.environ}
        env.update({"HOME": str(root / "home"), "TMPDIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime), "NO_COLOR": "1", "KILN_MACHINE_ID": "native-proof-server-" + root.name})
        launch("server", [str(binary), "--config", str(root / "kiln.toml"), "server"], env)
        wait("isolated server health", lambda: (root / "data/admin.token").is_file() and api("GET", "/healthz"))
        admin = (root / "data/admin.token").read_text().strip()
        api("GET", "/api/repos")  # Verifies this private token belongs to the listener.
        join = (root / "data/join.token").read_text().strip()
        name = "native-proof-" + uuid.uuid4().hex
        machine = hashlib.sha256(("kiln machine id\n" + name).encode()).hexdigest()[:32]
        gate = root / "go"
        heard = root / "work-here/.fake-claude-messages"
        private(heard, "")
        runner_env = {**env, "KILN_MACHINE_ID": name, "PATH": str(root / "bin") + os.pathsep + env.get("PATH", ""),
                      "FAKE_CLAUDE_LINES": "1", "FAKE_CLAUDE_EVERY_MS": "10", "FAKE_CLAUDE_GO": str(gate)}
        launch("supervisor", [str(before), "supervise", "--exe", str(before), "--", "agent", "--server", f"http://127.0.0.1:{http_port}", "--token", join, "--name", name,
                              "--state-dir", str(root / "runner-state"), "--work-dir", str(root / "runner-work")], runner_env)
        def connected():
            values = api("GET", "/api/agents")
            values = values if isinstance(values, list) else values.get("agents", [])
            return any(value.get("name") == machine and value.get("connected") for value in values)
        wait("owned runner connection", connected)
        api("PUT", "/api/runners/" + machine + "/headroom", {"enabled": False})
        wait("owned runner applied isolated fixture policy", lambda: api("GET", "/api/runners/" + machine + "/headroom").get("applied"))
        text = "owned-legacy-f555-native-proof-" + uuid.uuid4().hex
        private(root / "outgoing.json", json.dumps({"text": text, "route": "Spec.prompt; no publication/token"}) + "\n")
        private(root / "work-here/.fake-claude-completed", "")
        opened = api("POST", "/api/harness/sessions", {"target": "agent:" + machine, "spec": {"kind": "claude", "cwd": str(root / "work-here"), "program": str(root / "bin/claude"), "prompt": text}})
        require(isinstance(opened, list) and opened and isinstance(opened[0].get("id"), str), "the isolated session did not open")
        session = opened[0]["id"]
        def original_input():
            values = rows(heard)
            require(len(values) <= 1, "legacy bootstrap produced an extra native input")
            if not values:
                return None
            value = values[0]
            content = (value.get("message") or {}).get("content")
            actual = content if isinstance(content, str) else "\n".join(part.get("text", "") for part in content or [] if isinstance(part, dict))
            require(text in actual, "legacy provider did not receive the original prompt anchor")
            return value
        witness = wait("provider received actual legacy Spec.prompt", original_input)
        message = witness.get("uuid")
        require(isinstance(message, str) and message, "actual legacy input has no native UUID")
        body_hash = hashlib.sha256(json.dumps(witness["message"], sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        initial = wait("actual certified legacy source epoch", initial_identity)
        report["initial_certificate"] = initial["certificate"]
        report["legacy_original"] = {"uuid": message, "body_sha256": body_hash, "anchor_sha256": hashlib.sha256(text.encode()).hexdigest(), "route": "Spec.prompt", "server_publication": "none claimed"}
        provider = int(witness["pid"])
        identity = process_identity(provider)
        require(identity is not None, "provider PID is not the owned stand-in")
        providers[provider] = identity
        check("legacy_original_held_before_upgrade", not gate.exists() and not rows(root / "work-here/.fake-claude-completed"))
        report["initial_epoch"] = initial["epoch"]
        report["provider_pid"] = provider
        report["provider_start_identity"] = identity[:24]
        job = api("POST", "/api/updates/runners/" + machine)["id"]
        destination_epoch = initial["epoch"] + 1
        def upgrade_done():
            value = api("GET", "/api/updates/jobs/" + job)
            require(value.get("state") != "failed", "the owned native upgrade refused; inspect private fixture logs")
            return value.get("state") == "done"
        wait("upgrade success including session ACK", upgrade_done)
        certificate = wait("public verified destination certificate", destination_certificate)
        check("verified_exact_destination_epoch_one", destination_epoch == 1 and certificate["kind"] == "agent")
        report["destination_certificate"] = {key: certificate[key] for key in ("serial", "kind", "subject", "key", "verified", "refused")}
        after = wait("actual adopted legacy provider debt", held_debt)
        report["legacy_debt_held"] = after
        check("exact_epoch_one_debt_restored", destination_epoch == 1 and source_info().get("epoch") == 1)
        check("receiver_debt_present_while_gate_held", after["capacity"]["admitted"] == 1 and after["capacity"]["bytes"] > 0)
        check("not_completed_before_gate", not gate.exists() and not rows(root / "work-here/.fake-claude-completed"))
        check("same_native_process_alive_after_handover", process_identity(provider) == identity)
        # socket::agent_of also accepts survivors::MARK (KILN_AGENT), so remove it as well
        # as the person's terminal fallback. The acts() child must use process ownership.
        actor_argv = ["env", "-u", "KILN_TERM_SESSION", "-u", "KILN_AGENT", str(binary), "agents", "result", session, "--json"]
        actor_command = shlex.join(actor_argv)
        report["actor_authorization"] = {"command": actor_command, "provider_pid": provider,
                                          "terminal_and_agent_marks_removed": True, "result_gate_held": not gate.exists()}
        require(not gate.exists(), "provider result gate opened before actor authorization witness")
        private(root / "work-here" / (".fake-claude-acts-" + str(provider)), actor_command + "\n")
        def actor_answer():
            identity_logs(destination_epoch)
            matches = [row for row in rows(root / "work-here/.fake-claude-acted") if row.get("cmd") == actor_command]
            require(len(matches) <= 1, "the exact actor command ran more than once")
            return matches[0] if matches else None
        acted = wait("surviving provider child authorized agents.result", actor_answer)
        report["actor_authorization"]["record"] = acted
        check("surviving_provider_child_command_succeeded", acted.get("cmd") == actor_command and acted.get("pid") == provider and acted.get("code") == 0)
        actor_result = json.loads(acted["out"])
        check("actor_result_names_exact_session", actor_result.get("agent", {}).get("id") == session
              and actor_result["agent"].get("machine") == machine)
        check("actor_authorized_before_original_completion", not gate.exists() and held_debt() is not None and process_identity(provider) == identity)
        identity_logs(destination_epoch)
        gate.touch()
        def completed_debt():
            capacity = source_info().get("admission")
            if not isinstance(capacity, dict) or capacity.get("epoch") != destination_epoch or capacity.get("admitted") != 0 or capacity.get("bytes") != 0:
                return None
            return {"capacity": capacity, "observation": "public debt returned after releasing the exact original native result"}
        consumed = wait("actual native completion returns legacy receiver debt", completed_debt)
        completed = wait("provider records exact original completion", lambda: next((row for row in rows(root / "work-here/.fake-claude-completed") if row.get("uuid") == message), None))
        check("completion_correlates_exact_legacy_uuid", completed.get("pid") == provider and completed.get("uuid") == message)
        check("completion_returned_receiver_debt", consumed["capacity"]["admitted"] == 0 and consumed["capacity"]["bytes"] == 0)
        values = rows(heard)
        check("one_native_original_prompt_no_new_id", len(values) == 1 and values[0].get("uuid") == message)
        check("original_native_body_unchanged", hashlib.sha256(json.dumps(values[0]["message"], sort_keys=True, separators=(",", ":")).encode()).hexdigest() == body_hash)
        check("same_provider_after_real_completion", process_identity(provider) == identity)
        require(destination_certificate() is not None, "destination identity no longer verifies after completion")
        facts = identity_logs(destination_epoch)
        check("no_unexpected_or_refused_destination_epoch", not facts["unexpected_epochs"] and not facts["destination_refused"])
        report["legacy_debt_completed"] = consumed
        report["provider_completion"] = completed
        live_complete = True
        report["live_witnesses_complete"] = True
    except Exception as error:
        # Do not echo exception/request objects: they may contain the private fixture token.
        report["failure_type"] = type(error).__name__
        report["failure"] = str(error) if isinstance(error, Refused) else "unexpected fixture failure; inspect private logs"
    finally:
        def stop_owned(child):
            if child.poll() is None:
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                    child.wait(timeout=10)
                except (OSError, subprocess.TimeoutExpired):
                    if child.poll() is None:
                        try:
                            os.killpg(child.pid, signal.SIGKILL)
                        except OSError:
                            pass
                        child.wait(timeout=10)

        # Preserve the typed record until after observation: DELETE can forget a session's
        # inbox. These are only the groups/PIDs recorded by this disposable fixture.
        database_stats = []
        candidates = [root / "runner-state/records.db"]
        destination = agent_identity(report.get("destination_certificate", {}).get("subject"))
        if destination is not None:
            owner = root / "runner-state" if destination["instance"] in ("main", "legacy") else root / "runner-state/instances" / destination["instance"]
            if owner.resolve().is_relative_to((root / "runner-state").resolve()):
                candidates.append(owner / "records.db")
        for path in dict.fromkeys(candidates):
            try:
                st = path.stat()
                database_stats.append({"path": str(path), "bytes": st.st_size, "mtime_ns": st.st_mtime_ns, "mode": st.st_mode})
            except OSError as error:
                database_stats.append({"path": str(path), "stat_error": type(error).__name__})
        report["before_cleanup_db_stats"] = database_stats
        supervisors = [child for role, child in children if role == "supervisor"]
        for child in supervisors:
            stop_owned(child)
        for pid, identity in providers.items():
            if process_identity(pid) == identity:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        if report.get("destination_certificate") is not None:
            try:
                def writers_gone():
                    for child in supervisors:
                        try:
                            os.killpg(child.pid, 0)
                            return False
                        except ProcessLookupError:
                            continue
                    return True
                if live_complete:
                    while not writers_gone() and time.monotonic() < deadline:
                        time.sleep(0.1)
                require(writers_gone(), "owned runner writer group has not exited; typed persistence remains unverified")
                proof = legacy_pending()
                report["legacy_pending_after_writer_close"] = proof
                if live_complete:
                    pending = proof["pending"]
                    lease = pending.get("lease", {})
                    require(pending.get("origin") == "legacy_opaque" and pending.get("command") is None
                            and lease.get("message") == message and lease.get("token") is None
                            and lease.get("epoch") == destination_epoch and lease.get("stage") == "provider_flight",
                            "persisted legacy flight identity/origin/token/stage mismatch")
                    require(pending.get("disposition") == {"disposition": "consumed", "boundary": "provider_completed"},
                            "persisted exact legacy flight has no real completion proof")
                    check("typed_legacy_persistence_after_writer_close", True)
                    report["passed"] = True
                else:
                    report["typed_pending_observation"] = "failure snapshot after writer close; unconsumed/mismatched epoch retained as observed, never a pass"
            except Exception as error:
                report["passed"] = False
                report["typed_pending_observation"] = {"state": "failed or unavailable after owned writer close; no empty-journal inference", "error_type": type(error).__name__}
                if live_complete:
                    report["failure_type"] = type(error).__name__
                    report["failure"] = str(error) if isinstance(error, Refused) else "persisted legacy Pending could not be verified after writer close"
        if admin and session:
            try:
                api("DELETE", "/api/harness/sessions/" + session, allowed=(200, 202, 204, 404))
            except Exception:
                pass
        for role, child in reversed(children):
            if role != "supervisor":
                stop_owned(child)
        # Include identity events flushed during owned shutdown, after the final API read.
        if destination_epoch is not None:
            try:
                identity_logs(destination_epoch)
            except Exception as error:
                report["passed"] = False
                report["checks"]["no_unexpected_or_refused_destination_epoch"] = False
                report["failure_type"] = type(error).__name__
                report["failure"] = str(error) if isinstance(error, Refused) else "final owned identity log could not be verified"
        report["owned_groups_exited"] = all(child.poll() is not None for _, child in children)
        report["elapsed_seconds"] = time.monotonic() - started
        private(root / "result.json", json.dumps(report, indent=2) + "\n")
        print(json.dumps({"passed": report["passed"], "result": str(root / "result.json"), "checks": len(report["checks"]), "owned_groups_exited": report["owned_groups_exited"]}))
    return 0 if report["passed"] and report["owned_groups_exited"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
