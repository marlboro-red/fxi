import tempfile,pathlib,os,subprocess,json,time,socket,struct
base=pathlib.Path(tempfile.mkdtemp(prefix='fxi-audit-stop-')); root=base/'src';root.mkdir();(base/'run').mkdir()
env=dict(os.environ,FXI_INDEXES=str(base/'indexes'),FXI_SOCKET=str(base/'s'),XDG_RUNTIME_DIR=str(base/'run'),FXI_DEBOUNCE_MS='1000',FXI_MAX_BATCH_AGE_MS='2000')
bin=os.environ.get('FXI_AUDIT_BINARY',str(pathlib.Path(__file__).resolve().parents[3]/'target/release/fxi'))
for i in range(20):(root/f'{i}.rs').write_text('oldMarker\n')
subprocess.run([bin,'index',str(root)],env=env,capture_output=True,check=True)
log=open(base/'daemon.log','w'); daemon=subprocess.Popen([bin,'daemon','foreground','--watch'],env=env,stdout=log,stderr=log)
def req(obj):
 with socket.socket(socket.AF_UNIX) as s:
  s.settimeout(5);s.connect(str(base/'s'));data=json.dumps(obj).encode();s.sendall(struct.pack('<I',len(data))+data);n=struct.unpack('<I',s.recv(4))[0];out=b''
  while len(out)<n:out+=s.recv(n-len(out))
  return json.loads(out)
try:
 for _ in range(100):
  if (base/'s').exists():break
  time.sleep(.02)
 print('initial',req({'type':'Search','query':'oldMarker','root_path':str(root),'limit':0})['type'])
 time.sleep(2.4)
 (root/'new.rs').write_text('newAuditMarker\n')
 time.sleep(.1)
 print('shutdown',req({'type':'Shutdown'}));time.sleep(.4)
 print('daemon still alive after graceful ack:',daemon.poll() is None)
 # Wake blocking accept, allowing run() to cleanly join its worker threads.
 with socket.socket(socket.AF_UNIX) as s:s.connect(str(base/'s'))
 daemon.wait(timeout=5)
 p=subprocess.run([bin,'-l','newAuditMarker','-p',str(root)],env=env,capture_output=True)
 print('disk search',p.returncode,p.stdout.decode(),p.stderr.decode())
 print('fixture',base)
finally:
 if daemon.poll() is None:daemon.kill();daemon.wait()
 log.close()
