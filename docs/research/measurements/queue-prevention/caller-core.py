from pathlib import Path
import json,subprocess,os
root=Path("/private/tmp/kiln-queue-prevention-20261111");os.chdir(root)
evidence=root/"docs/research/measurements/queue-prevention"
exe="/Users/adalundhe/.cache/kiln-offline-state-fix-20261110/red-target/debug/deps/kiln-a2cd230deb814ff5"
selected=["harness::ops::", "harness::port::", "harness::mail_delivery::", "harness::wait::tests::a_quiet_wait", "harness::session::tests::accepted_queue_"]
with (evidence/"caller-core.log").open("wb") as f:r=subprocess.run(["nice","-n","10",exe,*selected,"--test-threads=1"],stdout=f,stderr=subprocess.STDOUT)
print(json.dumps({"exit_code":r.returncode,"log":str(evidence/"caller-core.log"),"capture":"check-1794387307432199000"}));raise SystemExit(r.returncode)
