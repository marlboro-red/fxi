"""Compare complete CLI calls and warm daemon requests on fixed source indexes."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import random
import statistics
import subprocess as sp
import tempfile
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--corpus', type=Path, required=True)
p.add_argument('--indexes', type=Path, required=True)
p.add_argument('--candidate-indexes', type=Path, required=True)
p.add_argument('--baseline', type=Path, required=True)
p.add_argument('--candidate', type=Path, required=True)
p.add_argument('--repetitions', type=int, default=31)
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
if a.repetitions < 1:
    p.error('repetitions must be positive')
spec = importlib.util.spec_from_file_location('daemon_bench', Path(__file__).with_name('compare-daemon-load.py'))
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)
root = a.corpus.resolve()
binaries = {'before': a.baseline.resolve(), 'after': a.candidate.resolve()}
indexes = {'before': a.indexes.resolve(), 'after': a.candidate_indexes.resolve()}
patterns = ['auditNonexistentSymbol94283', 'folio_wait_bit_common', 'struct file_operations']
base = Path(tempfile.mkdtemp(prefix='fxi-search-modes-'))
servers, environments, addresses = {}, {}, {}
result = {'corpus': str(root), 'indexes': {k: str(v) for k, v in indexes.items()},
          'mode': 'warm filesystem; daemon first request excludes process/socket startup; CLI includes complete process time',
          'candidate_query_local': True,
          'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
          'helper_sha256': hashlib.sha256(Path(spec.origin).read_bytes()).hexdigest(),
          'binaries': {k: {'path': str(v), 'sha256': hashlib.sha256(v.read_bytes()).hexdigest()} for k, v in binaries.items()},
          'rows': []}
try:
    for name in binaries:
        runtime = base / name
        runtime.mkdir()
        addresses[name] = runtime / 'daemon.sock'
        environments[name] = dict(os.environ, FXI_INDEXES=str(indexes[name]),
            FXI_SOCKET=str(addresses[name]), XDG_RUNTIME_DIR=str(runtime),
            FXI_QUERY_LOCAL='1' if name == 'after' else '0', FXI_NEGATIVE_ROUTING='0')
        with (runtime / 'server.log').open('w') as log:
            servers[name] = sp.Popen([str(binaries[name]), 'daemon', 'foreground'],
                cwd=root, env=environments[name], stdout=log, stderr=log)
        deadline = time.monotonic() + 20
        while not addresses[name].exists():
            assert servers[name].poll() is None, (runtime / 'server.log').read_text()
            if time.monotonic() > deadline:
                raise TimeoutError('daemon startup')
            time.sleep(.01)
    for pattern in patterns:
        oracle = sp.run(['rg', '-l', '-0', '--', pattern, '.'], cwd=root, capture_output=True, check=False)
        assert oracle.returncode in (0, 1), oracle.stderr
        expected = {path.removeprefix('./') for path in oracle.stdout.decode().split('\0') if path}
        first = {}
        for name in binaries:
            elapsed, paths = helper.request(addresses[name], root, pattern)
            assert paths == expected
            first[name] = elapsed
        samples = {f'{name}-{mode}': [] for name in binaries for mode in ['direct-cli', 'resident-cli', 'resident-api']}
        for rep in range(-1, a.repetitions):
            order = list(samples)
            random.Random(317 + rep).shuffle(order)
            for label in order:
                name, mode = label.split('-', 1)
                if mode == 'resident-api':
                    elapsed, paths = helper.request(addresses[name], root, pattern)
                else:
                    env = dict(environments[name])
                    if mode == 'direct-cli':
                        env['FXI_SOCKET'] = str(base / 'unused.sock')
                    started = time.perf_counter_ns()
                    process = sp.run([str(binaries[name]), '-l', '--color=never', f're:/{pattern}/', '-p', str(root)],
                        env=env, cwd=root, capture_output=True, timeout=120)
                    elapsed = (time.perf_counter_ns() - started) / 1e6
                    assert process.returncode == 0 and b'Daemon search failed' not in process.stderr, process.stderr
                    rows = process.stdout.decode().splitlines()
                    assert len(rows) == len(set(rows))
                    paths = {str(Path(path).relative_to(root)) if Path(path).is_absolute() else path.removeprefix('./') for path in rows}
                assert paths == expected, (label, pattern, paths ^ expected)
                if rep >= 0:
                    samples[label].append(elapsed)
        row = {'pattern': pattern, 'files': len(expected), 'first_request_ms': first,
               'modes': {k: {'median_ms': statistics.median(v), 'samples_ms': v} for k, v in samples.items()}}
        result['rows'].append(row)
        print(pattern, {k: round(v['median_ms'], 3) for k, v in row['modes'].items()}, flush=True)
    a.output.write_text(json.dumps(result, indent=2) + '\n')
finally:
    for process in servers.values():
        process.terminate()
    for process in servers.values():
        try:
            process.wait(timeout=10)
        except sp.TimeoutExpired:
            process.kill()
            process.wait()
