from pathlib import Path
import json,subprocess,os
os.chdir("/private/tmp/kiln-queue-prevention-20261111")
root=Path("docs/research/measurements/queue-prevention"); capture=root/"check-1794384968309236000"
r=json.loads((capture/"result.json").read_text());assert r["exit_code"]==0 and r["source_unchanged"]
exe="/Users/adalundhe/.cache/kiln-offline-state-fix-20261110/red-target/debug/deps/kiln-a2cd230deb814ff5"
listed=subprocess.check_output([exe,"--list"],text=True)
names=[line.split(": test")[0] for line in listed.splitlines() if ": test" in line]
keys=["harness::admission::","harness::inbox::","harness::mail::","harness::mail_transfer","harness::told::","harness::state_view::","harness::state_reads::","harness::queued::","harness::session::state_command_tests","harness::wait::tests::a_","harness::wait::tests::deferred_","store::storage","harness::core::tests::","harness::codex::tests::","harness::claude::tests::","skills::bridge::tests::","wire::","agent::tests::state_delivery","agent::tests::an_ordinary_state","agent::native_handover_tests::","records::turso::tests::","harness::inbox::accepting::","harness::host::transfer::","harness::session::input_journal_tests::"]
selected=[n for n in names if any(k in n for k in keys)]
(root/"combined-final-test-selection.json").write_text(json.dumps({"capture":str(capture),"selected":selected},indent=2)+"\n")
env=os.environ.copy();env["KILN_WRITE_TS"]="1";env["KILN_STORAGE_FIXTURE_JSON"]=str(root/"r18-storage-observations.jsonl")
with (root/"combined-final-focused-tests.log").open("wb") as out:r=subprocess.run(["nice","-n","10",exe,*selected,"--test-threads=1"],stdout=out,stderr=subprocess.STDOUT,env=env)
print(json.dumps({"selected":len(selected),"exit_code":r.returncode,"log":str(root/"combined-final-focused-tests.log")}));raise SystemExit(r.returncode)
