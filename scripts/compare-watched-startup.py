"""Compare first watched-root query latency against an existing current index.

Server process startup is excluded; index loading, initial reconciliation and
watcher registration are included. Each sample uses a fresh daemon.
"""
import argparse
import hashlib
import json
import os
import pathlib as P
import random
import statistics
import subprocess as sp
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--corpus', required=True, type=P.Path)
parser.add_argument('--indexes', required=True, type=P.Path)
parser.add_argument('--baseline', required=True, type=P.Path)
parser.add_argument('--candidate', required=True, type=P.Path)
parser.add_argument('--pattern', default='folio_wait_bit_common')
parser.add_argument('--repetitions', type=int, default=7)
parser.add_argument('--output', required=True, type=P.Path)
args = parser.parse_args()
root = args.corpus.resolve()
base = P.Path(tempfile.mkdtemp(prefix='fxi-watch-start-'))
expected = set(sp.check_output(['rg', '-l', '--color=never', args.pattern, '.'], cwd=root, text=True).splitlines())
expected = {path.removeprefix('./') for path in expected}
binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
rows = {name: [] for name in binaries}
for rep in range(args.repetitions):
    order = list(binaries)
    random.Random(612 + rep).shuffle(order)
    for name in order:
        runtime = base / f'{rep}-{name}'
        runtime.mkdir()
        socket = runtime / 'fxi.sock'
        env = {**os.environ, 'FXI_INDEXES': str(args.indexes.resolve()),
               'FXI_SOCKET': str(socket), 'XDG_RUNTIME_DIR': str(runtime)}
        binary = str(binaries[name])
        log_path = runtime / 'server.log'
        with log_path.open('w') as log:
            server = sp.Popen([binary, 'daemon', 'foreground', '--watch'], cwd=root, env=env,
                              stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 10
            while not socket.exists():
                assert server.poll() is None, log_path.read_text()
                if time.monotonic() > deadline:
                    raise TimeoutError('Daemon did not bind socket')
                time.sleep(.01)
            start = time.perf_counter_ns()
            result = sp.run([binary, '-l', '--color=never', f're:/{args.pattern}/', '-p', str(root)],
                            env=env, cwd=root, capture_output=True, timeout=120)
            elapsed = (time.perf_counter_ns() - start) / 1e6
            assert result.returncode == 0 and b'Daemon search failed' not in result.stderr, result.stderr.decode()
            assert server.poll() is None
            paths = [str(P.Path(p).relative_to(root)) if P.Path(p).is_absolute() else p.removeprefix('./')
                     for p in result.stdout.decode().splitlines()]
            assert len(paths) == len(set(paths)) and set(paths) == expected, paths
            assert 'starting file watcher' in log_path.read_text(), 'Query did not register a watcher'
            rows[name].append(elapsed)
            print(rep, name, round(elapsed, 3), 'ms', flush=True)
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except sp.TimeoutExpired:
                server.kill()
                server.wait()
args.output.write_text(json.dumps({'corpus': str(root), 'indexes': str(args.indexes.resolve()),
    'base': str(base), 'pattern': args.pattern, 'matching_files': len(expected),
    'harness_sha256': hashlib.sha256(P.Path(__file__).read_bytes()).hexdigest(),
    'tools': {name: {'binary': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                     'samples_ms': rows[name], 'median_ms': statistics.median(rows[name])}
              for name, binary in binaries.items()}}, indent=2))
