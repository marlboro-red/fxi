"""Run a timing command with this project's Rust analyzer temporarily suspended.

Other workspaces are left alone. Restore all suspended processes in finally,
including when the timing command fails or this wrapper receives SIGTERM.
This is macOS-oriented, like the /usr/bin/time -l benchmark harnesses.
"""
import os
import pathlib
import signal
import subprocess as sp
import sys

project = pathlib.Path(__file__).resolve().parent.parent
command = sys.argv[1:]
if command[:1] == ['--']:
    command = command[1:]
if not command:
    raise SystemExit('Usage: quiet-benchmark.py -- COMMAND [ARG ...]')

def processes():
    result = {}
    for line in sp.check_output(['ps', '-axo', 'pid=,ppid=,command='], text=True).splitlines():
        fields = line.split(None, 2)
        if len(fields) == 3:
            result[int(fields[0])] = (int(fields[1]), fields[2])
    return result

suspended = []
child = None

def terminate(signum, frame):
    if child is not None and child.poll() is None:
        child.terminate()
    raise SystemExit(128 + signum)

signal.signal(signal.SIGTERM, terminate)
try:
    for pid, (_, name) in processes().items():
        if not name.split()[0].endswith('/rust-analyzer'):
            continue
        cwd = sp.run(['lsof', '-a', '-p', str(pid), '-d', 'cwd', '-Fn'],
                     capture_output=True, text=True).stdout.splitlines()
        if not any(line.startswith('n') and pathlib.Path(line[1:]).resolve() == project for line in cwd):
            continue
        try:
            os.kill(pid, signal.SIGSTOP)
        except ProcessLookupError:
            continue
        suspended.append(pid)
        # Stop the parent first so it cannot launch another check while its
        # existing children (cargo/rustc/proc-macro helpers) are enumerated.
        pending = [pid]
        while pending:
            parent = pending.pop()
            for candidate, (ppid, _) in processes().items():
                if ppid == parent and candidate not in suspended:
                    try:
                        os.kill(candidate, signal.SIGSTOP)
                    except ProcessLookupError:
                        continue
                    suspended.append(candidate)
                    pending.append(candidate)
    print('Paused project analyzer processes:', suspended, flush=True)
    child = sp.Popen(command)
    raise SystemExit(child.wait())
finally:
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=10)
        except sp.TimeoutExpired:
            child.kill()
            child.wait()
    for pid in reversed(suspended):
        try:
            os.kill(pid, signal.SIGCONT)
        except ProcessLookupError:
            pass
    print('Restored project analyzer processes:', suspended, flush=True)
