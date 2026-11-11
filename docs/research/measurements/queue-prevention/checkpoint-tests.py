from pathlib import Path
import os,subprocess,json,sys
root=Path("/private/tmp/kiln-queue-prevention-20261111");os.chdir(root)
out=root/"docs/research/measurements/queue-prevention"
capture=out/"check-1794389677566664000"
assert json.loads((capture/"result.json").read_text())["source_unchanged"]
exe="/Users/adalundhe/.cache/kiln-offline-state-fix-20261110/red-target/debug/deps/kiln-a2cd230deb814ff5"
group=sys.argv[1]
keys={
 "core":["harness::ops::","harness::port::","harness::mail_delivery::","harness::wait::","harness::session::tests::accepted_queue_","reach::place::tests::lb03_real_epoch_file"],
 "consumers":["goals::","plans::","programs::"],
 "focused":["harness::admission::","harness::inbox::","harness::mail::","harness::mail_transfer","harness::told::","harness::state_view::","harness::state_reads::","harness::queued::","harness::session::state_command_tests","store::storage","harness::core::tests::","harness::codex::tests::","harness::claude::tests::","skills::bridge::tests::","wire::","agent::tests::state_delivery","agent::tests::an_ordinary_state","agent::native_handover_tests::","records::turso::tests::","harness::host::transfer::"]}[group]
listed=subprocess.check_output([exe,"--list"],text=True)
names=[line.split(": test")[0] for line in listed.splitlines() if ": test" in line]
selected=[name for name in names if any(key in name for key in keys)]
(out/("checkpoint-"+group+"-selection.json")).write_text(json.dumps({"capture":str(capture),"selected":selected},indent=2)+"\n")
env=os.environ.copy();env.pop("KILN_WRITE_TS",None)
with (out/("checkpoint-"+group+".log")).open("wb") as stream:r=subprocess.run(["nice","-n","10",exe,*selected,"--test-threads=1"],env=env,stdout=stream,stderr=subprocess.STDOUT)
print(json.dumps({"group":group,"count":len(selected),"exit_code":r.returncode}));raise SystemExit(r.returncode)
