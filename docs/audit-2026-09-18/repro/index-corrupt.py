import tempfile,pathlib,os,subprocess,json
base=pathlib.Path(tempfile.mkdtemp(prefix='fxi-audit-corrupt-'));root=base/'src';root.mkdir()
env=dict(os.environ,FXI_INDEXES=str(base/'indexes'),FXI_SOCKET=str(base/'absent.sock'),XDG_RUNTIME_DIR=str(base/'run'))
bin=os.environ.get('FXI_AUDIT_BINARY',str(pathlib.Path(__file__).resolve().parents[3]/'target/release/fxi'))
def run(args):
 p=subprocess.run([bin]+args,env=env,capture_output=True);print(json.dumps({'args':args,'status':p.returncode,'stdout':p.stdout.decode(errors='replace'),'stderr':p.stderr.decode(errors='replace')}));return p
for i in range(12):(root/f'{i}.rs').write_text('uniqueAuditNeedle\n')
run(['index',str(root),'--chunk-size','3'])
run(['-l','uniqueAuditNeedle','-p',str(root)])
container=next(p for p in (base/'indexes').iterdir() if p.is_dir());gen=container/'generations'/(container/'CURRENT').read_text().strip()
for name in ['grams.dict','grams.postings','tokens.dict','tokens.postings','tokens.positions']:
 p=gen/'segments'/'seg_0001'/name
 if p.exists():p.unlink()
run(['-l','uniqueAuditNeedle','-p',str(root)])
run(['compact',str(root)])
run(['-l','uniqueAuditNeedle','-p',str(root)])
print('fixture',base)
