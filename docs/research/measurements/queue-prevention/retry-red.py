from pathlib import Path
import json,subprocess
root=Path("/private/tmp/kiln-queue-prevention-20261111");out=root/"docs/research/measurements/queue-prevention/retry-red.log"
exe="/Users/adalundhe/.cache/kiln-offline-state-fix-20261110/red-target/debug/deps/kiln-a2cd230deb814ff5"
name="harness::mail::tests::preaccept_failure_cannot_turn_its_own_reservation_release_into_new_retry_credit"
r=subprocess.run(["nice","-n","10",exe,"--exact",name,"--nocapture","--test-threads=1"],cwd=root,capture_output=True,text=True,timeout=90)
text=r.stdout+r.stderr;out.write_text(text)
result={"exit_code":r.returncode,"executed_one":"running 1 test" in text,"expected_guard_failure":"a source's own failed reservation/release must not authorize the same publication again" in text,"log":str(out)}
(out.parent/"retry-red-result.json").write_text(json.dumps(result,indent=2)+"\n");print(json.dumps(result))
if not(result["executed_one"] and r.returncode!=0 and result["expected_guard_failure"]):print(text[-5000:])
raise SystemExit(0 if result["executed_one"] and r.returncode!=0 and result["expected_guard_failure"] else 1)
