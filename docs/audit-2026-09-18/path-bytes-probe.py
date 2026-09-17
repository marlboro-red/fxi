"""POSIX filename byte fidelity probe in isolated fixture."""
import json,os,pathlib,subprocess,tempfile,sys
base=pathlib.Path(tempfile.mkdtemp(prefix='fxi-path-audit-'));root=base/'repo';root.mkdir()
subprocess.run(['git','init','-q',str(root)],check=True)
try:
 fd=os.open(os.fsencode(root)+b'/bad-\xff.rs',os.O_CREAT|os.O_WRONLY,0o600)
except OSError as error:
 result=dict(status='unsupported_on_this_filesystem',error=str(error))
 pathlib.Path('docs/audit-2026-09-18/path-bytes-observations.json').write_text(json.dumps(result,indent=2)+'\n')
 print(result);sys.exit(0)
os.write(fd,b'byteNameNeedle commonMarker\n');os.close(fd)
(root/'bad-\ufffd.rs').write_text('unicodeNameNeedle commonMarker\n')
env={**os.environ,'FXI_INDEXES':str(base/'indexes'),'FXI_SOCKET':str(base/'socket')};binary=str(pathlib.Path('target/release/fxi').resolve());rows=[]
for args in [['index','--force',str(root)],['-l','byteNameNeedle','-p',str(root)],['-l','unicodeNameNeedle','-p',str(root)],['-l','commonMarker','-p',str(root)]]:
 r=subprocess.run([binary,*args],env=env,capture_output=True);rows.append(dict(args=args,code=r.returncode,stdout=r.stdout.decode(errors='replace'),stderr=r.stderr.decode(errors='replace')))
pathlib.Path('docs/audit-2026-09-18/path-bytes-observations.json').write_text(json.dumps(dict(base=str(base),rows=rows),indent=2)+'\n')
print(rows)
