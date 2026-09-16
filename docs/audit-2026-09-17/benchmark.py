"""Controlled-corpus audit benchmark. Requires shallow Redis and tgrep clones in /tmp.
Run from fxi root after release builds. Uses isolated indexes and daemons.
"""
from collections import Counter
import subprocess as sp, pathlib as P, os, time, json, statistics, random, shutil, tempfile, argparse, hashlib, re
parser = argparse.ArgumentParser()
parser.add_argument("--output", default="docs/audit-2026-09-17/benchmark-results.json")
parser.add_argument("--source", default="/tmp/fxi-audit-redis")
parser.add_argument("--fxi", default="target/release/fxi")
parser.add_argument("--fxi-revision", help="Revision of an alternate --fxi binary")
parser.add_argument("--suite", choices=["redis", "python", "linux"], default="redis")
parser.add_argument("--repetitions", type=int, default=9)
parser.add_argument("--prepared-corpus", action="store_true", help="Use --source as an already-controlled corpus")
parser.add_argument("--source-revision", help="Original commit for a prepared corpus")
parser.add_argument("--build-repetitions", type=int, default=1)
parser.add_argument("--build-only", action="store_true")
parser.add_argument("--modes", nargs="+", choices=["direct", "server"], default=["direct", "server"])
parser.add_argument("--output-mode", choices=["files", "count"], default="files")
parser.add_argument("--queries", nargs="+", choices=["selective", "absent", "phrase", "common", "alternation", "internal_literal", "insensitive"])
args = parser.parse_args()
harness_sha256 = hashlib.sha256(P.Path(__file__).read_bytes()).hexdigest()
fxi=str(P.Path(args.fxi).resolve()); tg='/tmp/fxi-audit-tgrep/target/release/tgrep'
base=P.Path(tempfile.mkdtemp(prefix='fxi-audit-bench-')); root=P.Path(args.source).resolve() if args.prepared_corpus else base/'corpus'
if not args.prepared_corpus: root.mkdir()
runtime=base/'runtime'; runtime.mkdir()
env={**os.environ,'FXI_INDEXES':str(base/'indexes'),'FXI_SOCKET':str(runtime/'fxi.sock'),'XDG_RUNTIME_DIR':str(runtime)}
source=P.Path(args.source); count=0; size=0; manifest=[]
for name in sp.check_output(['rg','--files'],cwd=source,text=True).splitlines():
 p=source/name
 if p.is_symlink() or p.suffix not in {'.c','.h','.tcl','.py','.md','.rs'}: continue
 if any(c.startswith('.') or c in {'target','node_modules','venv','__pycache__'} for c in p.relative_to(source).parts): continue
 data=p.read_bytes()
 if not data or len(data)>10_000_000 or b'\0' in data: continue
 try: data.decode('utf8')
 except UnicodeDecodeError: continue
 if not args.prepared_corpus:
  dest=root/name; dest.parent.mkdir(parents=True,exist_ok=True); dest.write_bytes(data)
 count+=1; size+=len(data); manifest.append((name,hashlib.sha256(data).hexdigest()))
sp.run(['git','init','-q',str(root)],check=True)
def run(cmd):
 start=time.perf_counter_ns(); p=sp.run(cmd,cwd=root,env=env,stdout=sp.PIPE,stderr=sp.PIPE); elapsed=(time.perf_counter_ns()-start)/1e6
 if p.returncode not in ((0,) if cmd[0] == fxi else (0,1)): raise RuntimeError((cmd,p.returncode,p.stderr.decode()))
 if b'Daemon search failed' in p.stderr: raise RuntimeError(('Daemon fallback invalidates server measurement', cmd, p.stderr.decode()))
 records=[]
 for line in p.stdout.decode().splitlines():
  path, suffix = (line.rsplit(':', 1) if args.output_mode == 'count' else (line, None))
  path = str(P.Path(path).relative_to(root)) if P.Path(path).is_absolute() else path.removeprefix('./')
  records.append((path, int(suffix)) if suffix is not None else path)
 paths=Counter(records)
 return elapsed,paths
builds={tool: {'samples': []} for tool in ['fxi','tgrep']}
for rep in range(args.build_repetitions):
 order=[('fxi',[fxi,'index','--force',str(root)]),('tgrep',[tg,'index','--force',str(root)])]
 random.Random(rep).shuffle(order)
 for tool,cmd in order:
  start=time.perf_counter(); p=sp.run(['/usr/bin/time','-l',*cmd],cwd=root,env=env,capture_output=True,text=True,check=True)
  builds[tool]['samples'].append({'seconds':time.perf_counter()-start,'resource_output':p.stderr,'max_rss_bytes':int(re.search(r'(\d+)\s+maximum resident set size',p.stderr).group(1))})
for tool,build in builds.items():
 build['seconds']=statistics.median(x['seconds'] for x in build['samples'])
 build['max_rss_bytes']=statistics.median(x['max_rss_bytes'] for x in build['samples'])
 build['resource_output']=build['samples'][-1]['resource_output']
queries=[('selective','raxFind'),('absent','auditNonexistentSymbol94283'),('phrase','static void'),('common','return'),('alternation','raxFind|dictRehash'),('internal_literal','.*raxFind'),('insensitive','serverassert')]
if args.suite == 'python':
 queries=[('selective','PyObject_GenericGetAttr'),('absent','auditNonexistentSymbol94283'),('phrase','static void'),('common','return'),('alternation','PyObject_GenericGetAttr|PyUnicode_DecodeUTF8'),('internal_literal','.*PyObject_GenericGetAttr'),('insensitive','pyobject_genericgetattr')]
if args.suite == 'linux':
 queries=[('selective','folio_wait_bit_common'),('absent','auditNonexistentSymbol94283'),('phrase','struct file_operations'),('common','return'),('alternation','folio_wait_bit_common|bpf_prog_select_runtime'),('internal_literal','.*folio_wait_bit_common'),('insensitive','blk_mq_alloc_request')]
if args.queries:
 queries=[q for q in queries if q[0] in args.queries]
rows=[]; processes=[]
try:
 for mode in ([] if args.build_only else args.modes):
  if mode=='server':
   for name,cmd in [('fxi',[fxi,'daemon','foreground']),('tgrep',[tg,'serve','--no-watch',str(root)])]:
    log=open(base/(name+'-server.log'),'w'); processes.append(sp.Popen(cmd,cwd=root,env=env,stdout=log,stderr=log))
   time.sleep(2)
   assert all(p.poll() is None for p in processes), "Benchmark server exited before measurement"
  for label,pat in queries:
   flags=['-i'] if label=='insensitive' else []
   output_flag='-l' if args.output_mode == 'files' else '-c'
   cmds={'fxi':[fxi,*flags,output_flag,'--color=never','re:/'+pat+'/','-p',str(root)],'tgrep':[tg,*flags,output_flag,'--color=never',pat,str(root)],'rg':['rg',*flags,output_flag,'--color=never',pat,'.']}
   outputs={k:run(c)[1] for k,c in cmds.items()}; expected=outputs['rg']; assert all(paths==expected for paths in outputs.values()), (label, outputs); timings={k:[] for k in cmds}
   for rep in range(args.repetitions):
    order=list(cmds); random.Random(rep).shuffle(order)
    for k in order:
     cmd=cmds[k].copy()
     if mode=='server':
      variant=pat+'(?:)'*(rep+1)
      cmd[cmd.index('re:/'+pat+'/') if k=='fxi' else cmd.index(pat)]='re:/'+variant+'/' if k=='fxi' else variant
     if mode=='server': assert all(p.poll() is None for p in processes), 'Benchmark server exited before sample'
     ms,paths=run(cmd); assert paths==outputs[k]; timings[k].append(ms)
     if mode=='server': assert all(p.poll() is None for p in processes), 'Benchmark server exited during sample'
   row={'mode':mode,'query':label,'pattern':pat,'files':len(expected),'tools':{k:{'median_ms':statistics.median(v),'samples_ms':v,'missing':sorted(expected-outputs[k]),'extra':sorted(outputs[k]-expected)} for k,v in timings.items()}}
   if mode=='server':
    row['fxi_repeated_query_ms']=[run(cmds['fxi'])[0] for _ in range(args.repetitions)]
    row['server_rss_kib']={name:int(sp.check_output(['ps','-o','rss=','-p',str(proc.pid)],text=True).strip()) for name,proc in zip(['fxi','tgrep'], processes)}
   rows.append(row)
   print(mode, label, {k: round(v["median_ms"], 2) for k, v in row["tools"].items()}, flush=True)
finally:
 for p in processes:
  p.terminate()
 for p in processes:
  try:p.wait(timeout=5)
  except sp.TimeoutExpired:p.kill();p.wait()
result={'harness_sha256':harness_sha256,'fxi_binary_sha256':hashlib.sha256(P.Path(fxi).read_bytes()).hexdigest(),'manifest_sha256':hashlib.sha256(json.dumps(sorted(manifest)).encode()).hexdigest(),'suite':args.suite,'output_mode':args.output_mode,'fxi_commit':args.fxi_revision or (sp.check_output(['git','rev-parse','HEAD'],text=True).strip() if args.fxi=='target/release/fxi' else 'external-binary'),'base':str(base),'corpus':str(root),'tgrep_commit':sp.check_output(['git','rev-parse','HEAD'],cwd='/tmp/fxi-audit-tgrep',text=True).strip(),'source_commit':args.source_revision or sp.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'files':count,'bytes':size,'builds':builds,'index_bytes':{'fxi':sum(p.stat().st_size for p in (base/'indexes').rglob('*') if p.is_file()),'tgrep':sum(p.stat().st_size for p in (root/'.tgrep').rglob('*') if p.is_file())},'rows':rows}
P.Path(args.output).write_text(json.dumps(result,indent=2))
print(json.dumps({k:v for k,v in result.items() if k not in ['rows','builds']},indent=2))
for row in rows:print(row['mode'],row['query'],row['files'],{k:(round(v['median_ms'],2),len(v['missing']),len(v['extra'])) for k,v in row['tools'].items()})
