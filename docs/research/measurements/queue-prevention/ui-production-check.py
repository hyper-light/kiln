from pathlib import Path
import hashlib,json,os,subprocess,time
root=Path.cwd();out=root/"docs/research/measurements/queue-prevention";record=out/("ui-production-"+str(time.time_ns()));record.mkdir()
files=[p for p in (root/"ui/src").rglob("*") if p.is_file()]+[root/"ui/package.json",root/"ui/package-lock.json"]
def snap():return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
before=snap();(record/"before.json").write_text(json.dumps(before,indent=2)+"\n")
env=os.environ.copy();env["KILN_UI_SERVER"]="require";results=[]
for name,cmd in [("install",["npm","ci","--ignore-scripts"]),("types",["npm","run","typecheck"]),("lint",["npm","run","lint"]),("build",["npm","run","build"])]:
 with (record/(name+".log")).open("wb") as log:r=subprocess.run(["nice","-n","10",*cmd],cwd=root/"ui",env=env,stdout=log,stderr=subprocess.STDOUT)
 results.append({"name":name,"exit_code":r.returncode})
 if r.returncode:break
after=snap();result={"results":results,"source_unchanged":before==after,"record":str(record)};(record/"result.json").write_text(json.dumps(result,indent=2)+"\n");print(json.dumps(result));raise SystemExit(next((r["exit_code"] for r in results if r["exit_code"]),0))
