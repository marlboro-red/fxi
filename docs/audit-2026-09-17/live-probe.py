import pathlib as P,tempfile,subprocess as S,os,time,json
base=P.Path(tempfile.mkdtemp(prefix='fxi-audit-live-')); root=base/'corpus';root.mkdir(); run=base/'run';run.mkdir(); fxi=str(P.Path('target/release/fxi').resolve()); env={**os.environ,'FXI_INDEXES':str(base/'indexes'),'FXI_SOCKET':str(run/'s'),'XDG_RUNTIME_DIR':str(run),'FXI_DELTA_FLUSH_SECS':'1','FXI_DEBOUNCE_MS':'50'}
def call(*args):return S.run([fxi,*args],env=env,capture_output=True,text=True)

for i in range(10): (root/f'filler{i}.txt').write_text('unrelated filler text\n')
(root/'a.txt').write_text('K foo\n');print(call('index',str(root)).returncode);p=call('foo','-p',str(root),'--color=never');print('UNICODE',p.returncode,repr(p.stderr))
(root/'a.txt').write_text('originalneedle\n');call('index','--force',str(root));log=open(base/'daemon.log','w');d=S.Popen([fxi,'daemon','foreground','--watch'],env=env,stdout=log,stderr=log)
try:
 time.sleep(1);print('FIRST',call('-l','originalneedle','-p',str(root)).stdout.strip());time.sleep(1)
 (root/'a.txt').write_text('modifiedneedle\n');time.sleep(3)
 print('MODIFIED',call('-l','modifiedneedle','-p',str(root)).stdout.strip())
 meta=next((base/'indexes').glob('*/docs.bin'));data=meta.read_bytes(); print('WATCH_MTIME',max(int.from_bytes(data[20+i*30:28+i*30],'little') for i in range(int.from_bytes(data[:4],'little'))),'EXPECTED_SECONDS',int(time.time()))
 (root/'a.txt').unlink();time.sleep(3)
 print('DELETED_CONTENT',call('-l','modifiedneedle','-p',str(root)).stdout.strip());print('DELETED_FILE',call('-l','ext:txt','-p',str(root)).stdout.strip());print('LOG', (base/'daemon.log').read_text())
finally:d.terminate();d.wait(timeout=5)
