"""Paired process-startup calibration; an empty index is not a physical lower bound."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import statistics
import subprocess as sp
import tempfile
import time

p = argparse.ArgumentParser(__doc__)
p.add_argument('--baseline', type=Path, required=True)
p.add_argument('--candidate', type=Path, required=True)
p.add_argument('--repetitions', type=int, default=31)
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
if a.repetitions < 1:
    p.error('repetitions must be positive')
binaries = {'before': a.baseline.resolve(), 'after': a.candidate.resolve()}
with tempfile.TemporaryDirectory(prefix='fxi-cli-startup-') as temporary:
    base = Path(temporary)
    root = base / 'source'
    root.mkdir()
    env = dict(os.environ, FXI_INDEXES=str(base / 'indexes'), FXI_APP_DATA=str(base / 'data'),
               FXI_SOCKET=str(base / 'unused.sock'), XDG_RUNTIME_DIR=temporary,
               FXI_QUERY_LOCAL='1', FXI_GENERATION_ROUTING='1')
    sp.run([str(binaries['after']), 'index', str(root)], env=env, check=True, capture_output=True)
    arguments = {'version': ['--version'], 'empty_checked_absence':
                 ['-l', 're:/auditNonexistentSymbol94283/', '-p', str(root)]}
    result = {'mode': 'complete process time; warm filesystem; same empty checked index',
              'repetitions': a.repetitions,
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'binaries': {k: {'path': str(v), 'sha256': hashlib.sha256(v.read_bytes()).hexdigest()}
                           for k, v in binaries.items()}, 'rows': []}
    for name, args in arguments.items():
        samples = {variant: [] for variant in binaries}
        expected = None
        for repetition in range(-2, a.repetitions):
            order = list(binaries)
            random.Random(2911 + repetition).shuffle(order)
            for variant in order:
                started = time.perf_counter_ns()
                proc = sp.run([str(binaries[variant]), *args], env=env, capture_output=True, timeout=30)
                elapsed = (time.perf_counter_ns() - started) / 1e6
                assert proc.returncode == 0, proc.stderr
                if name == 'empty_checked_absence':
                    assert not proc.stdout, proc.stdout
                if expected is None:
                    expected = proc.stdout
                assert proc.stdout == expected
                if repetition >= 0:
                    samples[variant].append(elapsed)
        row = {'case': name, 'tools': {k: {'median_ms': statistics.median(v), 'samples_ms': v}
                                      for k, v in samples.items()}}
        result['rows'].append(row)
        print(name, {k: v['median_ms'] for k, v in row['tools'].items()}, flush=True)
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(json.dumps(result, indent=2) + '\n')
