"""Compare two FXI daemons using one immutable index; validate every sample."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--corpus', type=Path, required=True)
parser.add_argument('--indexes', type=Path, required=True)
parser.add_argument('--baseline', type=Path, required=True)
parser.add_argument('--candidate', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--repetitions', type=int, default=11)
args = parser.parse_args()
if args.repetitions < 1:
    parser.error('repetitions must be positive')
root = args.corpus.resolve()
base = Path(tempfile.mkdtemp(prefix='fxi-warm-files-'))
binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
servers, logs, envs = {}, {}, {}
rows = []

def paths(output):
    result = []
    for line in output.decode().splitlines():
        path = Path(line)
        result.append(str(path.relative_to(root)) if path.is_absolute() else line.removeprefix('./'))
    assert len(result) == len(set(result)), 'Duplicate paths'
    return set(result)

try:
    for name, binary in binaries.items():
        runtime = base / name
        runtime.mkdir()
        envs[name] = {**os.environ, 'FXI_INDEXES': str(args.indexes.resolve()),
                      'FXI_SOCKET': str(runtime / 'fxi.sock'), 'XDG_RUNTIME_DIR': str(runtime)}
        logs[name] = (runtime / 'server.log').open('w')
        servers[name] = subprocess.Popen([str(binary), 'daemon', 'foreground'], cwd=root,
                                        env=envs[name], stdout=logs[name], stderr=logs[name])
    deadline = time.monotonic() + 20
    while not all(Path(env['FXI_SOCKET']).exists() for env in envs.values()):
        assert all(server.poll() is None for server in servers.values()), 'Daemon exited'
        if time.monotonic() > deadline:
            raise TimeoutError('Daemon socket startup')
        time.sleep(.02)
    for pattern in ['folio_wait_bit_common', 'auditNonexistentSymbol94283', 'struct file_operations', 'return']:
        oracle = subprocess.run(['rg', '-l', '--color=never', pattern, '.'], cwd=root, capture_output=True, timeout=120)
        assert oracle.returncode in (0, 1), oracle.stderr
        expected = paths(oracle.stdout)
        samples = {name: [] for name in binaries}
        for rep in range(-1, args.repetitions):
            order = list(binaries)
            random.Random(1729 + rep).shuffle(order)
            for name in order:
                assert servers[name].poll() is None
                variant = pattern + '(?:)' * (rep + 2)
                start = time.perf_counter_ns()
                result = subprocess.run([str(binaries[name]), '-l', '--color=never', f're:/{variant}/', '-p', str(root)],
                                        cwd=root, env=envs[name], capture_output=True, timeout=120)
                elapsed = (time.perf_counter_ns() - start) / 1e6
                assert result.returncode == 0 and b'Daemon search failed' not in result.stderr, result.stderr
                assert servers[name].poll() is None
                assert paths(result.stdout) == expected, (name, pattern)
                if rep >= 0:
                    samples[name].append(elapsed)
        row = {'pattern': pattern, 'files': len(expected), 'tools': {
            name: {'median_ms': statistics.median(values), 'samples_ms': values}
            for name, values in samples.items()}}
        rows.append(row)
        print(pattern, {name: entry['median_ms'] for name, entry in row['tools'].items()}, flush=True)
finally:
    for server in servers.values():
        server.terminate()
    for server in servers.values():
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()
    for log in logs.values():
        log.close()
args.output.write_text(json.dumps({'base': str(base), 'corpus': str(root), 'indexes': str(args.indexes.resolve()),
    'binaries': {name: {'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
                 for name, binary in binaries.items()},
    'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'rows': rows}, indent=2) + '\n')
