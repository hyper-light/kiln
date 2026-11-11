#!/usr/bin/env python3
"""Review artifact only. Default: read-only check. Root must authorize --start separately."""
import argparse
import hashlib
import json
import mmap
import os
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import time


COMMIT = "f555736bd887a4196c8841564cb9b4d6c1ede95f"
CANONICAL_SHA = "1ae92b19b6acf6a2f5e91e7efeffe0f249236fa26c30c5c5d6b71d73f2a04ad9"
INSTANCE = "15429e44-08b4-43b0-a9b8-71652114543e"
LABEL = "io.github.hyper-light.kiln-agent-15429e44"
OLD_LABEL = "io.github.hyper-light.kiln-agent"
OLD_PLIST_SHA = "cb5732af709c640926d2eec159e85f2bfbcf3e505fe77e7779c373babc95c18e"
TASK_HOME = Path("/Users/adalundhe")
BIN = TASK_HOME / ".local/bin/kiln"
STATE = TASK_HOME / ".local/share/kiln/agent"
OLD_PLIST = TASK_HOME / "Library/LaunchAgents" / (OLD_LABEL + ".plist")
NEW_PLIST = OLD_PLIST.with_name(LABEL + ".plist")
OWN = STATE / "instances" / INSTANCE
MARKER = OWN / "manual-rollout.json"


class Refusal(Exception):
    pass


def require(condition, code):
    if not condition:
        raise Refusal(code)


def sha(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def command(argv):
    # Never print subprocess output: service errors can contain its private arguments/environment.
    return subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL, timeout=30).returncode


def exclusive(path, data, mode):
    fd = os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    with os.fdopen(fd, "wb") as destination:
        destination.write(data)
        destination.flush()
        os.fsync(destination.fileno())


def clean_arguments(argv):
    # This is the exact old LaunchAgent shape read during preflight, not shell text.
    require(isinstance(argv, list) and all(isinstance(a, str) for a in argv), "invalid_arguments")
    require(argv[1:5] == ["supervise", "--exe", str(BIN), "--"], "old_supervisor_shape_changed")
    child = argv[5:]
    require(child and child[0] == "agent", "old_child_is_not_agent")
    remove = {"--instance", "--token", "--join-token"}
    result = []
    index = 0
    while index < len(child):
        word = child[index]
        if word in remove:
            require(index + 1 < len(child), "missing_argument_value")
            index += 2
            continue
        if any(word.startswith(key + "=") for key in remove):
            index += 1
            continue
        result.append(word)
        index += 1
    # Explicit instance/state flags must win over an old environment default.
    result.extend(["--instance", INSTANCE])
    require(not any(a == "--state-dir" or a.startswith("--state-dir=") for a in result),
            "unexpected_old_state_argument")
    result.extend(["--state-dir", str(STATE)])
    return [str(BIN), "supervise", "--exe", str(BIN), "--"] + result


def check():
    require(os.getuid() == 501 and Path.home() == TASK_HOME, "wrong_user_or_host_home")
    require(not BIN.is_symlink() and stat.S_ISREG(BIN.lstat().st_mode), "canonical_not_regular")
    require(sha(BIN) == CANONICAL_SHA, "canonical_changed_since_review")
    with BIN.open("rb") as source:
        with mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ) as image:
            require(image.find(COMMIT.encode("ascii")) >= 0, "compiled_commit_bytes_missing")
    require(command(["/usr/bin/codesign", "--verify", "--strict", str(BIN)]) == 0, "codesign_refused")
    require(command([str(BIN), "--version"]) == 0, "version_failed")
    require(shutil.disk_usage(TASK_HOME).free >= 30 * 1024**3 + BIN.stat().st_size, "disk_floor")
    for name in ["agent.pem", "agent.key", "ca.pem"]:
        require((STATE / name).is_file(), "existing_enrollment_missing")
    require(not OLD_PLIST.is_symlink() and OLD_PLIST.stat().st_uid == os.getuid(), "old_plist_owner")
    before = sha(OLD_PLIST)
    require(before == OLD_PLIST_SHA, "old_plist_changed_since_root_check")
    with OLD_PLIST.open("rb") as source:
        old = plistlib.load(source)
    require(old.get("Label") == OLD_LABEL, "old_label_changed")
    argv = clean_arguments(old.get("ProgramArguments"))
    environment = dict(old.get("EnvironmentVariables", {}))
    require(all(isinstance(k, str) and isinstance(v, str) for k, v in environment.items()), "invalid_environment")
    for key in ["KILN_JOIN_TOKEN", "KILN_INSTANCE", "KILN_SUPERVISOR", "KILN_SUPERVISE_CHILD",
                "KILN_TAKE_OVER_FROM", "KILN_GENERATION", "KILN_SUPERVISED"]:
        environment.pop(key, None)
    require(environment.get("KILN_STATE_DIR", str(STATE)) == str(STATE), "unexpected_state_environment")
    domain = next((d for d in ["gui/501", "user/501"]
                   if command(["/bin/launchctl", "print", d + "/" + OLD_LABEL]) == 0), None)
    require(domain is not None, "old_service_not_loaded")
    require(sha(OLD_PLIST) == before and sha(BIN) == CANONICAL_SHA, "preflight_changed")
    new = {"Label": LABEL, "ProgramArguments": argv, "EnvironmentVariables": environment,
           "RunAtLoad": True, "KeepAlive": {"SuccessfulExit": False},
           "StandardOutPath": str(OWN / "kiln.log"), "StandardErrorPath": str(OWN / "kiln.log")}
    if "SoftResourceLimits" in old:
        new["SoftResourceLimits"] = old["SoftResourceLimits"]
    return new, domain, before


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--start", action="store_true", help="MUTATES: create/load only the reviewed new instance")
    mode.add_argument("--rollback", action="store_true", help="MUTATES: unload only the reviewed new label")
    parser.add_argument("--server-confirmed-no-owned-work", action="store_true")
    args = parser.parse_args()
    if args.rollback:
        require(args.server_confirmed_no_owned_work, "root_must_verify_no_new_owned_work")
        require(os.getuid() == 501 and Path.home() == TASK_HOME, "wrong_user_or_host_home")
        require(MARKER.is_file() and not MARKER.is_symlink(), "not_this_scripts_instance")
        saved = json.loads(MARKER.read_text())
        require(saved.get("instance") == INSTANCE and saved.get("label") == LABEL, "ownership_marker_mismatch")
        require(saved.get("domain") in ["gui/501", "user/501"], "invalid_owned_domain")
        require(not NEW_PLIST.is_symlink() and sha(NEW_PLIST) == saved.get("plist_sha256"), "owned_plist_changed")
        require(command(["/bin/launchctl", "disable", saved["domain"] + "/" + LABEL]) == 0,
                "owned_disable_failed")
        require(command(["/bin/launchctl", "bootout", saved["domain"] + "/" + LABEL]) == 0,
                "owned_bootout_failed")
        # Preserve instance state/certificate/logs and its plist for diagnosis/recovery.
        print(json.dumps({"rolled_back_label": LABEL, "old_label_untouched": True, "state_preserved": str(OWN)}))
        return
    new, domain, old_sha = check()
    require(not NEW_PLIST.exists() and not NEW_PLIST.is_symlink() and not OWN.exists(), "new_instance_collision")
    require(command(["/bin/launchctl", "print", domain + "/" + LABEL]) != 0, "new_label_already_loaded")
    if not args.start:
        print(json.dumps({"check": "passed", "instance": INSTANCE, "new_label": LABEL,
                          "domain": domain, "canonical_sha256": CANONICAL_SHA, "old_plist_sha256": old_sha,
                          "argument_count": len(new["ProgramArguments"]),
                          "environment_count": len(new["EnvironmentVariables"]), "mutations": 0}))
        return
    OWN.mkdir(mode=0o700, parents=False, exist_ok=False)
    exclusive(OWN / "instance.json", json.dumps({"id": INSTANCE, "created_at": int(time.time())}).encode(), 0o644)
    encoded = plistlib.dumps(new, fmt=plistlib.FMT_XML)
    exclusive(NEW_PLIST, encoded, 0o600)
    exclusive(MARKER, json.dumps({"instance": INSTANCE, "label": LABEL, "domain": domain,
                                 "old_plist_sha256": old_sha,
                                 "plist_sha256": hashlib.sha256(encoded).hexdigest()}).encode(), 0o600)
    for directory in [OWN, OWN.parent, NEW_PLIST.parent]:
        descriptor = os.open(str(directory), os.O_RDONLY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    require(sha(OLD_PLIST) == old_sha and sha(BIN) == CANONICAL_SHA, "inputs_changed_before_load")
    require(command(["/bin/launchctl", "bootstrap", domain, str(NEW_PLIST)]) == 0, "new_bootstrap_failed")
    require(sha(OLD_PLIST) == old_sha, "old_plist_changed_during_load")
    print(json.dumps({"loaded_label": LABEL, "instance": INSTANCE, "verification": "pending",
                      "old_label_untouched": True, "log": str(OWN / "kiln.log")}))


if __name__ == "__main__":
    try:
        main()
    except Refusal as error:
        print(json.dumps({"refused": str(error)}))
        raise SystemExit(1)
    except Exception:
        # No traceback/argv/environment/PKI material in ordinary evidence.
        print(json.dumps({"refused": "operation_failed_preserve_new_state_and_inspect_privately"}))
        raise SystemExit(1)
