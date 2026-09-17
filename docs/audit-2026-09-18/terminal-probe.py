"""Unix pseudo-terminal evidence: initialization failure must restore terminal."""
import fcntl,json,os,pathlib,pty,select,struct,subprocess,tempfile,termios,time
base=pathlib.Path(tempfile.mkdtemp(prefix='fxi-terminal-audit-'))
master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',24,100,0,0))
before=termios.tcgetattr(slave)
def setup():
 os.setsid();fcntl.ioctl(0,termios.TIOCSCTTY,0)
env={**os.environ,'TERM':'xterm-256color','FXI_INDEXES':str(base/'indexes'),'FXI_SOCKET':str(base/'socket')}
binary=str(pathlib.Path('target/release/fxi').resolve())
state_path=base/'state.json'
child="""import subprocess,termios,json,sys
before=termios.tcgetattr(0)
r=subprocess.run([sys.argv[1],'search',sys.argv[2]])
after=termios.tcgetattr(0)
open(sys.argv[3],'w').write(json.dumps(dict(exit_code=r.returncode,before_echo=bool(before[3]&termios.ECHO),after_echo=bool(after[3]&termios.ECHO),before_canonical=bool(before[3]&termios.ICANON),after_canonical=bool(after[3]&termios.ICANON))))
termios.tcsetattr(0,termios.TCSANOW,before)
"""
p=subprocess.Popen(['python3','-c',child,binary,str(base/'does-not-exist'),str(state_path)],stdin=slave,stdout=slave,stderr=slave,env=env,preexec_fn=setup)
data=b'';deadline=time.monotonic()+5
while time.monotonic()<deadline:
 if select.select([master],[],[],.1)[0]:
  data+=os.read(master,65536)
 if p.poll() is not None:break
if p.poll() is None:p.terminate()
p.wait(timeout=5)
result=json.loads(state_path.read_text())
result.update(entered_alternate=b'\x1b[?1049h' in data,left_alternate=b'\x1b[?1049l' in data,output=data.decode(errors='replace'))
pathlib.Path('docs/audit-2026-09-18/terminal-observations.json').write_text(json.dumps(result,indent=2)+'\n');print(result)
os.close(master);os.close(slave)
