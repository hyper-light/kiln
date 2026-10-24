#!/usr/bin/env python3
"""A GitHub App's installation token for one repository, written to a file only its owner can
read: for `ci/conformance/probes.py github --token-file` (docs/ci.md W22-AC03) on the machine
where the App's private key is. The App's JWT is signed with that key by `openssl`, the
repository's installation is looked up, and a token is asked for with only the permissions the
probes use (contents and workflows to push them, actions and checks to read their runs). Neither the key nor
the token is printed.

    python3 -I ci/conformance/github_app_token.py --app-id 123456 \\
        --key ~/Downloads/kiln.2026-10-20.private-key.pem --repo you/kiln-probes \\
        --out ~/.config/kiln-probes/token

The App must be installed on the repository, with those permissions granted; a token lasts an hour.
"""

import argparse
import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

# What the probes do on GitHub: push their workflows, then read their runs, jobs, logs and
# annotations (their check runs').
PERMISSIONS = {"contents": "write", "workflows": "write", "actions": "read", "checks": "read", "metadata": "read"}


def b64(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def app_jwt(app_id, key):
    """The App's own JWT (RS256, nine minutes, issued a minute back for clock skew)."""
    now = int(time.time())
    signing = b64(json.dumps({"alg": "RS256", "typ": "JWT"}).encode()) + "." + b64(json.dumps({"iat": now - 60, "exp": now + 540, "iss": str(app_id)}).encode())
    signed = subprocess.run(["openssl", "dgst", "-sha256", "-sign", key], input=signing.encode(), capture_output=True, check=True).stdout
    return signing + "." + b64(signed)


def call(url, bearer, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers={"Authorization": f"Bearer {bearer}", "Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "kiln-probes"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        sys.exit(f"{method} {url}: {e.code} {e.read().decode(errors='replace')[:300]}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--app-id", required=True, help="the App's ID, from its settings page on GitHub")
    ap.add_argument("--key", required=True, help="the App's private key (.pem)")
    ap.add_argument("--repo", required=True, help="owner/name of the repository the App is installed on")
    ap.add_argument("--out", required=True, help="where the token goes (made readable by you alone)")
    ap.add_argument("--api", default="https://api.github.com")
    a = ap.parse_args()
    owner_repo = a.repo.strip("/")
    jwt = app_jwt(a.app_id, os.path.expanduser(a.key))
    installation = call(f"{a.api}/repos/{owner_repo}/installation", jwt)
    token = call(f"{a.api}/app/installations/{installation['id']}/access_tokens", jwt, "POST", {"repositories": [owner_repo.split("/")[-1]], "permissions": PERMISSIONS})
    out = os.path.expanduser(a.out)
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(token["token"])
    granted = ", ".join(f"{k} {v}" for k, v in sorted((token.get("permissions") or {}).items()))
    print(f"an installation token for {owner_repo} (until {token.get('expires_at')}) is in {out}: {granted}")


if __name__ == "__main__":
    main()
