"""Fresh small-corpus audit benchmark. Requires shallow Redis and tgrep clones in /tmp.
Run from fxi root after release builds. Uses isolated indexes and daemons.
"""
import subprocess as sp, pathlib as P, os, time, json, statistics, random, shutil, tempfile, argparse
parser = argparse.ArgumentParser()
parser.add_argument("--output", default="docs/audit-2026-09-17/benchmark-results.json")
args = parser.parse_args()
fxi=str(P.Path('target/release/fxi').resolve()); tg='/tmp/fxi-audit-tgrep/target/release/tgrep'
base=P.Path(tempfile.mkdtemp(prefix='fxi-audit-bench-')); root=base/'corpus'; root.mkdir(); runtime=base/'runtime'; runtime.mkdir()
env={**os.environ,'FXI_INDEXES':str(base/'indexes'),'FXI_SOCKET':str(runtime/'fxi.sock'),'XDG_RUNTIME_DIR':str(runtime)}
source=P.Path('/tmp/fxi-audit-redis'); count=0; size=0
for name in sp.check_output(['rg','--files'],cwd=source,text=True).splitlines():
 p=source/name
 if p.is_symlink() or p.suffix not in {'.c','.h','.tcl','.py','.md','.rs'}: continue
 if any(c.startswith('.') or c in {'target','node_modules','venv','__pycache__'} for c in p.relative_to(source).parts): continue
 data=p.read_bytes()
 if not data or len(data)>10_000_000 or b'\0' in data: continue
 try: data.decode('utf8')
 except UnicodeDecodeError: continue
 dest=root/name; dest.parent.mkdir(parents=True,exist_ok=True); dest.write_bytes(data); count+=1; size+=len(data)
sp.run(['git','init','-q',str(root)],check=True)
def run(cmd):
 start=time.perf_counter_ns(); p=sp.run(cmd,cwd=root,env=env,stdout=sp.PIPE,stderr=sp.PIPE); elapsed=(time.perf_counter_ns()-start)/1e6
 if p.returncode not in (0,1): raise RuntimeError((cmd,p.returncode,p.stderr.decode()))
 paths={str(P.Path(x).relative_to(root)) if P.Path(x).is_absolute() else x.removeprefix('./') for x in p.stdout.decode().splitlines()}
 return elapsed,paths
builds={}
for tool,cmd in [('fxi',[fxi,'index',str(root)]),('tgrep',[tg,'index',str(root)])]:
 start=time.perf_counter(); p=sp.run(['/usr/bin/time','-l',*cmd],cwd=root,env=env,capture_output=True,text=True,check=True); builds[tool]={'seconds':time.perf_counter()-start,'resource_output':p.stderr}
queries=[('selective','raxFind'),('absent','auditNonexistentSymbol94283'),('phrase','static void'),('common','return'),('alternation','raxFind|dictRehash'),('internal_literal','.*raxFind'),('insensitive','serverassert')]
rows=[]; processes=[]
try:
 for mode in ['direct','server']:
  if mode=='server':
   for name,cmd in [('fxi',[fxi,'daemon','foreground']),('tgrep',[tg,'serve','--no-watch',str(root)])]:
    log=open(base/(name+'-server.log'),'w'); processes.append(sp.Popen(cmd,cwd=root,env=env,stdout=log,stderr=log))
   time.sleep(2)
  for label,pat in queries:
   flags=['-i'] if label=='insensitive' else []
   cmds={'fxi':[fxi,*flags,'-l','--color=never','re:/'+pat+'/','-p',str(root)],'tgrep':[tg,*flags,'-l','--color=never',pat,str(root)],'rg':['rg',*flags,'-l','--color=never',pat,'.']}
   outputs={k:run(c)[1] for k,c in cmds.items()}; expected=outputs['rg']; timings={k:[] for k in cmds}
   for rep in range(9):
    order=list(cmds); random.Random(rep).shuffle(order)
    for k in order:
     cmd=cmds[k].copy()
     if mode=='server':
      variant=pat+'(?:)'*(rep+1)
      cmd[cmd.index('re:/'+pat+'/') if k=='fxi' else cmd.index(pat)]='re:/'+variant+'/' if k=='fxi' else variant
     ms,paths=run(cmd); assert paths==outputs[k]; timings[k].append(ms)
   row={'mode':mode,'query':label,'pattern':pat,'files':len(expected),'tools':{k:{'median_ms':statistics.median(v),'samples_ms':v,'missing':sorted(expected-outputs[k]),'extra':sorted(outputs[k]-expected)} for k,v in timings.items()}}
   if mode=='server': row['fxi_result_cache_ms']=[run(cmds['fxi'])[0] for _ in range(9)]
   rows.append(row)
finally:
 for p in processes:
  p.terminate()
 for p in processes:
  try:p.wait(timeout=5)
  except sp.TimeoutExpired:p.kill();p.wait()
result={'fxi_commit':sp.check_output(['git','rev-parse','HEAD'],text=True).strip(),'base':str(base),'source_commit':sp.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'files':count,'bytes':size,'builds':builds,'index_bytes':{'fxi':sum(p.stat().st_size for p in (base/'indexes').rglob('*') if p.is_file()),'tgrep':sum(p.stat().st_size for p in (root/'.tgrep').rglob('*') if p.is_file())},'rows':rows}
P.Path(args.output).write_text(json.dumps(result,indent=2))
print(json.dumps({k:v for k,v in result.items() if k not in ['rows','builds']},indent=2))
for row in rows:print(row['mode'],row['query'],row['files'],{k:(round(v['median_ms'],2),len(v['missing']),len(v['extra'])) for k,v in row['tools'].items()})
