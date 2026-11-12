from pathlib import Path
import os,subprocess,json
root=Path("/private/tmp/kiln-queue-prevention-20261111");os.chdir(root)
evidence=root/"docs/research/measurements/queue-prevention"
exe="/Users/adalundhe/.cache/kiln-offline-state-fix-20261110/red-target/debug/deps/kiln-a2cd230deb814ff5"
with (evidence/"consumer-check.log").open("wb") as f:r=subprocess.run(["nice","-n","10",exe,"goals::","plans::","programs::","--test-threads=1"],stdout=f,stderr=subprocess.STDOUT)
print(json.dumps({"exit_code":r.returncode,"log":str(evidence/"consumer-check.log"),"compiled_capture":"check-1794384968309236000"}));raise SystemExit(r.returncode)
