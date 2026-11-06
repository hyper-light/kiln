"""Lab driver (goal 4 steps 1 and 7): real Safari through safaridriver's WebDriver, on a Mac.

  python3 safari.py <page url> [port]

Starts safaridriver on the port, opens one session, loads the page, waits for its result and
prints it with Safari's version. Needs Safari's "Allow Remote Automation" (safaridriver --enable).
"""

import json
import subprocess
import sys
import time
import urllib.request

url = sys.argv[1]
port = int(sys.argv[2]) if len(sys.argv) > 2 else 18444
drv = subprocess.Popen(["safaridriver", "-p", str(port)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
base = "http://127.0.0.1:%d" % port


def call(method, path, body=None):
    req = urllib.request.Request(base + path, method=method, data=None if body is None else json.dumps(body).encode(), headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read() or b"{}")


try:
    for _ in range(50):
        try:
            call("GET", "/status")
            break
        except Exception:
            time.sleep(0.1)
    s = call("POST", "/session", {"capabilities": {"alwaysMatch": {"browserName": "safari"}}})
    sid = s["value"]["sessionId"]
    version = s["value"]["capabilities"].get("browserVersion")
    try:
        call("POST", "/session/%s/url" % sid, {"url": url})
        result = None
        for _ in range(300):
            r = call("POST", "/session/%s/execute/sync" % sid, {"script": "return window.__result || null", "args": []})
            if r.get("value") and r["value"].get("done"):
                result = r["value"]
                break
            time.sleep(0.1)
        print(json.dumps({"browser": "safari", "version": version, "result": result}))
    finally:
        call("DELETE", "/session/%s" % sid)
except Exception as e:
    body = ""
    if hasattr(e, "read"):
        try:
            body = e.read().decode(errors="replace")[:600]
        except Exception:
            pass
    err = drv.stderr.read1(2000).decode(errors="replace") if drv.poll() is not None else ""
    print(json.dumps({"browser": "safari", "error": str(e)[:400], "body": body, "driver": err}))
finally:
    drv.terminate()
