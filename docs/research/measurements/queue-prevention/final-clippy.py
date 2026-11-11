from pathlib import Path
import json,subprocess,os,shutil
root=Path("/private/tmp/kiln-queue-prevention-20261111");os.chdir(root)
assert shutil.disk_usage(root).free >= 34*1024**3
out=root/"docs/research/measurements/queue-prevention/final-clippy.log"
env=os.environ.copy();env.update(CARGO_TARGET_DIR=str(Path.home()/".cache/kiln-offline-state-fix-20261110/red-target"),CARGO_BUILD_JOBS="2",KILN_UI_SERVER="off",CARGO_NET_OFFLINE="true")
with out.open("wb") as f:r=subprocess.run(["nice","-n","10","cargo","clippy","--locked","-p","kiln","--lib","--","-D","warnings"],env=env,stdout=f,stderr=subprocess.STDOUT)
print(json.dumps({"exit_code":r.returncode,"log":str(out)}));raise SystemExit(r.returncode)
