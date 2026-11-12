"""Coordinator focused compile capture, with before/after source identity witnesses."""
from pathlib import Path
import hashlib,json,os,shutil,subprocess,time
root=Path.cwd(); evidence=root/'docs/research/measurements/queue-prevention'
run=evidence/('check-'+str(time.time_ns()));run.mkdir()
def snapshot():
 paths=subprocess.check_output(['git','ls-files','--cached','--others','--exclude-standard'],text=True).splitlines()
 paths=[p for p in paths if p.startswith('crates/') and p.endswith(('.rs','.sql','.toml'))]+['Cargo.toml','Cargo.lock']
 result={}
 for name in paths:
  p=root/name
  if p.is_file():result[name]=hashlib.sha256(p.read_bytes()).hexdigest()
 return result
before=snapshot();(run/'before.json').write_text(json.dumps(before,indent=2)+'\n')
free=shutil.disk_usage(root).free
if free<(30+int(os.environ.get('KILN_CHECK_SCRATCH_GIB','1')))*1024**3:raise SystemExit('Mac30GiB floor +1GiB scratch unavailable')
env=os.environ.copy();env.update(CARGO_TARGET_DIR=str(Path.home()/'.cache/kiln-offline-state-fix-20261110/red-target'),CARGO_BUILD_JOBS='2',KILN_UI_SERVER='off',CARGO_NET_OFFLINE='true')
command=['nice','-n','10','cargo',*(['test','--no-run'] if env.get('KILN_CHECK_MODE')=='test' else ['check']),'--locked','-p','kiln','--lib','--message-format=json']
with (run/'compiler.jsonl').open('wb') as out:completed=subprocess.run(command,env=env,stdout=out,stderr=subprocess.STDOUT)
after=snapshot();(run/'after.json').write_text(json.dumps(after,indent=2)+'\n')
errors=[]
for line in (run/'compiler.jsonl').read_text(errors='replace').splitlines():
 try:v=json.loads(line)
 except ValueError:continue
 m=v.get('message',{})
 if m.get('level')=='error':errors.append({'message':m.get('message'),'rendered':m.get('rendered')})
result={'exit_code':completed.returncode,'source_unchanged':before==after,'errors':errors,'free_before':free,'free_after':shutil.disk_usage(root).free,'command':command}
(run/'result.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps({'record':str(run),'exit_code':completed.returncode,'source_unchanged':before==after,'error_count':len(errors),'free_after':result['free_after']}))
for error in errors:print(error['rendered'] or error['message'])
raise SystemExit(completed.returncode)
