#!/usr/bin/env python3
"""Coordinator-only, disposable live direct-SCM proof. Never targets an installed service.

Uses the existing transfers.rs/common CLI setup and fake-claude helper. It does not build,
install, change production, run a model, or claim cancellation/crash-boundary coverage.

The post-handover acts() child removes both terminal and agent environment identities:
socket.rs must recognize the surviving provider's process group/ancestry, then authorize
agents.result through its actual runner certificate. Public Listed identity fields come
from api.rs/identities.rs; no private key, actor mark or direct admin operation substitutes
for that child call. This witnesses only the owned Unix fake-provider handover, not every
provider/platform or production actor. Root owns executing this live fixture.
"""
import argparse
from contextlib import contextmanager
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
    parser.add_argument("--before-binary", type=Path)
    parser.add_argument("--before-sha256")
    parser.add_argument("--before-source-receipt", type=Path)
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
    prior = args.before_binary.resolve(strict=True) if args.before_binary else None
    if prior:
        require(args.before_sha256 is not None and args.before_source_receipt is not None, "a prior artifact needs its exact hash and source receipt")
        require(sha(prior) == args.before_sha256, "the prior artifact hash differs")
        source = json.loads(args.before_source_receipt.read_bytes())
        require(source.get("exit_code") == 0 and source.get("source_unchanged") is True and source.get("sha256") == args.before_sha256,
                "the prior artifact lacks a successful unchanged-source build witness")
    else:
        require(args.before_sha256 is None and args.before_source_receipt is None, "a prior hash/receipt requires its artifact")
    root = args.out.absolute()
    root.parent.mkdir(parents=True, exist_ok=True)
    require(shutil.disk_usage(root.parent).free >= args.keep_free_bytes + binary.stat().st_size * 2,
            "fixture disk copies would cross the coordinator's operational floor")
    root.mkdir(mode=0o700)
    started = time.monotonic()
    deadline = started + args.timeout_seconds
    children = []
    runtime = Path(tempfile.mkdtemp(prefix="kn-", dir="/private/tmp"))
    providers = {}
    admin = None
    session = None
    handoff = None
    http_port = port(socket.SOCK_STREAM)
    quic_port = port(socket.SOCK_DGRAM)
    report = {"binary_sha256": args.sha256, "source_receipt_sha256": sha(args.source_receipt),
              "source_receipt": str(args.source_receipt.resolve()), "out": str(root),
              "kind": "actual-isolated-cli-native-scm", "checks": {}, "passed": False, "runtime": str(runtime)}

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

    @contextmanager
    def ledger():
        database = root / "data" / "kiln.db"
        connection = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=1)
        try:
            yield connection
        finally:
            connection.close()

    def mail(message):
        with ledger() as database:
            row = database.execute("SELECT revision,generation,epoch,disposition,body FROM agent_mail WHERE recipient=? AND id=?", (session, message)).fetchone()
        return None if row is None else {"revision": row[0], "generation": row[1], "epoch": row[2],
                                        "disposition": row[3], "body_sha256": hashlib.sha256(row[4].encode()).hexdigest()}

    def committed():
        with ledger() as database:
            row = database.execute("SELECT record FROM agent_mail_handoffs WHERE session=? AND phase='committed' ORDER BY ordinal DESC LIMIT 1", (session,)).fetchone()
        return None if row is None else json.loads(row[0])

    def process_identity(pid):
        result = subprocess.run(["ps", "-p", str(pid), "-o", "lstart=", "-o", "command="], capture_output=True, text=True, env={**os.environ, "LC_ALL": "C"}, timeout=5)
        text = result.stdout.strip()
        return text if result.returncode == 0 and str(root / "bin" / "claude") in text else None

    def identity_logs(expected_epoch):
        raw = (root / "supervisor.log").read_bytes()
        facts = identity_log_observations(raw.decode("utf-8", errors="replace"), session, (initial["epoch"], expected_epoch))
        report["identity_log"] = {**facts, "sha256": hashlib.sha256(raw).hexdigest()}
        require(not facts["unexpected_epochs"] and not facts["destination_refused"],
                "owned supervisor requested or refused an unexpected destination identity epoch; inspect private log")
        return facts

    def destination_certificate(handoff):
        epoch = handoff["manifest"]["destination_epoch"]
        identity_logs(epoch)
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
            require(parsed["epoch"] in (initial["epoch"], epoch), "public agent identity has an unexpected epoch")
            if parsed["epoch"] == epoch:
                require(parsed["machine"] == handoff["destination"]["machine"]
                        and parsed["instance"] == handoff["destination"]["instance"], "destination certificate names a different owner")
                if certificate.get("refused") is None and isinstance(certificate.get("verified"), str) and certificate["verified"]:
                    matches.append(certificate)
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
        private(root / "bin" / "claude", helper_text.replace(original, replacement))
        os.chmod(root / "bin" / "claude", 0o700)
        report["helper_original_sha256"] = sha(helper)
        report["helper_fixture_sha256"] = sha(root / "bin" / "claude")
        before = root / "kiln-before"
        with open(prior or binary, "rb") as source, open(before, "xb") as destination:
            shutil.copyfileobj(source, destination, 1024 * 1024)
            if prior is None:
                destination.write(b"\0")
        os.chmod(before, 0o700)
        report["before_sha256"] = sha(before)
        require(report["before_sha256"] != args.sha256, "the before/after artifacts must differ to exercise upgrade")
        if prior:
            report["before_source_receipt_sha256"] = sha(args.before_source_receipt)
            report["before_source_receipt"] = str(args.before_source_receipt.resolve())
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
        with ledger() as database:
            check("new_mail_schema_present", database.execute("SELECT 1 FROM sqlite_master WHERE name='agent_mail_handoffs'").fetchone() is not None)
        join = (root / "data/join.token").read_text().strip()
        name = "native-proof-" + uuid.uuid4().hex
        machine = hashlib.sha256(("kiln machine id\n" + name).encode()).hexdigest()[:32]
        gate = root / "go"
        heard = root / "work-here/.fake-claude-messages"
        private(heard, "")
        runner_env = {**env, "KILN_MACHINE_ID": name, "PATH": str(root / "bin") + os.pathsep + env.get("PATH", ""),
                      "FAKE_CLAUDE_LINES": "1", "FAKE_CLAUDE_EVERY_MS": "10", "FAKE_CLAUDE_GO": str(gate)}
        launch("supervisor", [str(binary), "supervise", "--exe", str(before), "--", "agent", "--server", f"http://127.0.0.1:{http_port}", "--token", join, "--name", name,
                              "--state-dir", str(root / "runner-state"), "--work-dir", str(root / "runner-work")], runner_env)
        def connected():
            values = api("GET", "/api/agents")
            values = values if isinstance(values, list) else values.get("agents", [])
            return any(value.get("name") == machine and value.get("connected") for value in values)
        wait("owned runner connection", connected)
        api("PUT", "/api/runners/" + machine + "/headroom", {"enabled": False})
        wait("owned runner applied isolated fixture policy", lambda: api("GET", "/api/runners/" + machine + "/headroom").get("applied"))
        opened = api("POST", "/api/harness/sessions", {"target": "agent:" + machine, "spec": {"kind": "claude", "cwd": str(root / "work-here"), "program": str(root / "bin/claude")}})
        require(isinstance(opened, list) and opened and isinstance(opened[0].get("id"), str), "the isolated session did not open")
        session = opened[0]["id"]
        wait("actual source bootstrap admission", lambda: (api("GET", "/api/harness/sessions/" + session).get("admission") or {}).get("target", 0) > 0)
        text = "owned-native-scm-proof-" + uuid.uuid4().hex
        key = "native-proof-" + uuid.uuid4().hex
        private(root / "outgoing.json", json.dumps({"session": session, "text": text, "key": key}) + "\n")
        publication = api("POST", f"/api/harness/agents/{session}/send", {"text": text, "key": key})
        message = publication.get("message") or publication.get("publication", {}).get("id")
        require(isinstance(message, str) and message, "send did not name its immutable message")
        witness = wait("provider received the actual original", lambda: next((row for row in rows(heard) if row.get("uuid") == message), None))
        provider = int(witness["pid"])
        identity = process_identity(provider)
        require(identity is not None, "provider PID is not the owned stand-in")
        providers[provider] = identity
        initial = wait("source admitted exact message before provider completion", lambda: (value if (value := mail(message)) and value["disposition"] == "admitted" else None))
        report["initial_mail"] = initial
        report["provider_pid"] = provider
        report["provider_start_identity"] = identity[:24]
        job = api("POST", "/api/updates/runners/" + machine)["id"]
        def live_committed():
            status = api("GET", "/api/updates/jobs/" + job)
            require(status.get("state") != "failed", "the owned native upgrade refused before journal handover; inspect private fixture logs")
            return committed()
        handoff = wait("nonempty direct journal committed", live_committed)
        check("nonempty_manifest", handoff["manifest"]["count"] > 0)
        check("exact_source_epoch", handoff["manifest"]["source_epoch"] == initial["epoch"])
        check("next_epoch", handoff["manifest"]["destination_epoch"] == initial["epoch"] + 1)
        def upgrade_done():
            value = api("GET", "/api/updates/jobs/" + job)
            require(value.get("state") != "failed", "the owned native upgrade refused; inspect private fixture logs")
            return value.get("state") == "done"
        wait("upgrade success including session ACK", upgrade_done)
        after = wait("same attempt rebound to new epoch", lambda: (value if (value := mail(message)) and value["epoch"] == handoff["manifest"]["destination_epoch"] else None))
        check("same_revision_and_generation", (after["revision"], after["generation"]) == (initial["revision"], initial["generation"]))
        check("original_body_unchanged", after["body_sha256"] == initial["body_sha256"])
        check("not_consumed_before_gate", after["disposition"] != "consumed")
        check("same_native_process_alive_after_handover", process_identity(provider) == identity)
        headers = []
        for path in (root / "runner-state").rglob("mail-transfer-*"):
            if path.is_file() and path.stat().st_size <= 512 * 1024:
                try:
                    value = json.loads(path.read_text())
                except (ValueError, UnicodeError):
                    continue
                if value.get("manifest") == handoff["manifest"] and value.get("saved", {}).get("id") == session:
                    headers.append(value)
        check("source_and_destination_durable_native_headers", len(headers) >= 2)
        check("snapshot_preserves_native_pid", all((value.get("handed") or {}).get("process", {}).get("pid") == provider for value in headers))
        certificate = wait("public verified destination certificate", lambda: destination_certificate(handoff))
        check("verified_exact_destination_epoch_one", handoff["manifest"]["destination_epoch"] == 1 and certificate["kind"] == "agent")
        report["destination_certificate"] = {key: certificate[key] for key in ("serial", "kind", "subject", "key", "verified", "refused")}
        # socket::agent_of also accepts survivors::MARK (KILN_AGENT), so remove it as well
        # as the person's terminal fallback. The acts() child must use process ownership.
        actor_argv = ["env", "-u", "KILN_TERM_SESSION", "-u", "KILN_AGENT", str(binary), "agents", "result", session, "--json"]
        actor_command = shlex.join(actor_argv)
        report["actor_authorization"] = {"command": actor_command, "provider_pid": provider,
                                          "terminal_and_agent_marks_removed": True, "result_gate_held": not gate.exists()}
        require(not gate.exists(), "provider result gate opened before actor authorization witness")
        private(root / "work-here" / (".fake-claude-acts-" + str(provider)), actor_command + "\n")
        def actor_answer():
            identity_logs(handoff["manifest"]["destination_epoch"])
            matches = [row for row in rows(root / "work-here/.fake-claude-acted") if row.get("cmd") == actor_command]
            require(len(matches) <= 1, "the exact actor command ran more than once")
            return matches[0] if matches else None
        acted = wait("surviving provider child authorized agents.result", actor_answer)
        report["actor_authorization"]["record"] = acted
        check("surviving_provider_child_command_succeeded", acted.get("cmd") == actor_command and acted.get("pid") == provider and acted.get("code") == 0)
        actor_result = json.loads(acted["out"])
        check("actor_result_names_exact_session", actor_result.get("agent", {}).get("id") == session
              and actor_result["agent"].get("machine") == machine)
        check("actor_authorized_before_original_completion", not gate.exists() and mail(message) == after and process_identity(provider) == identity)
        identity_logs(handoff["manifest"]["destination_epoch"])
        gate.touch()
        consumed = wait("destination observed native completion", lambda: (value if (value := mail(message)) and value["disposition"] == "consumed" else None))
        check("completion_kept_exact_attempt", (consumed["revision"], consumed["generation"], consumed["epoch"]) == (after["revision"], after["generation"], after["epoch"]))
        api("POST", f"/api/harness/agents/{session}/send", {"text": text, "key": key})
        check("one_native_prompt_after_exact_retry", sum(row.get("uuid") == message for row in rows(heard)) == 1)
        check("retry_kept_terminal_receipt", mail(message) == consumed)
        require(destination_certificate(handoff) is not None, "destination identity no longer verifies after completion")
        facts = identity_logs(handoff["manifest"]["destination_epoch"])
        check("no_unexpected_or_refused_destination_epoch", not facts["unexpected_epochs"] and not facts["destination_refused"])
        report["handoff"] = handoff
        report["final_mail"] = consumed
        report["passed"] = True
    except Exception as error:
        # Do not echo exception/request objects: they may contain the private fixture token.
        report["failure_type"] = type(error).__name__
        report["failure"] = str(error) if isinstance(error, Refused) else "unexpected fixture failure; inspect private logs"
    finally:
        if admin and session:
            try:
                api("DELETE", "/api/harness/sessions/" + session, allowed=(200, 202, 204, 404))
            except Exception:
                pass
        for role, child in reversed(children):
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
        for pid, identity in providers.items():
            if process_identity(pid) == identity:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        # Include identity events flushed during owned shutdown, after the final API read.
        if handoff is not None:
            try:
                identity_logs(handoff["manifest"]["destination_epoch"])
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
