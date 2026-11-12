from pathlib import Path
import json,subprocess,os
root=Path("/private/tmp/kiln-queue-prevention-20261111")
os.chdir(root/"ui")
results=[]
for name,args in [("types",["npm","run","typecheck"]),("lint",["npm","run","lint"])]:
 with (root/"docs/research/measurements/queue-prevention"/("latest-ui-"+name+".log")).open("wb") as f:r=subprocess.run(args,stdout=f,stderr=subprocess.STDOUT)
 results.append({"name":name,"code":r.returncode})
 if r.returncode:break
print(json.dumps(results));raise SystemExit(any(r["code"] for r in results))
