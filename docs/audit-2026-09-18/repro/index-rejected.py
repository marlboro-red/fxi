import tempfile,pathlib,os,subprocess,json
base=pathlib.Path(tempfile.mkdtemp(prefix='fxi-audit-rejected-'));root=base/'src';root.mkdir()
env=dict(os.environ,FXI_INDEXES=str(base/'indexes'),FXI_SOCKET=str(base/'absent.sock'))
bin=os.environ.get('FXI_AUDIT_BINARY',str(pathlib.Path(__file__).resolve().parents[3]/'target/release/fxi'))
def run(args):
 p=subprocess.run([bin]+args,env=env,capture_output=True);print(json.dumps({'args':args,'status':p.returncode,'stdout':p.stdout.decode(errors='replace'),'stderr':p.stderr.decode(errors='replace')}))
for i in range(12):(root/f'{i}.rs').write_text('original\n')
run(['index',str(root)])
f=root/'new.rs';f.write_text('uniqueAuditRetryMarker\n');f.chmod(0)
try:run(['index',str(root)])
finally:f.chmod(0o644)
run(['index',str(root)])
run(['-l','uniqueAuditRetryMarker','-p',str(root)])
print('fixture',base)
