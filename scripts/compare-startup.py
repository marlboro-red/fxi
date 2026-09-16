"""Interleave one-shot searches using saved binaries and one immutable index.

The prepared corpus must already be indexed by FXI and (optionally) tgrep.
Result sets are checked against ripgrep outside each timed subprocess region.
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
parser.add_argument('--tgrep', type=P.Path)
parser.add_argument('--repetitions', type=int, default=31)
parser.add_argument('--output', required=True, type=P.Path)
parser.add_argument('--count', action='store_true')
parser.add_argument('--literal', action='store_true', help='Compare plain FXI identifier queries with case-insensitive fixed strings')
parser.add_argument('--patterns', nargs='+')
args = parser.parse_args()
if args.repetitions < 1:
    parser.error('repetitions must be positive')
root = args.corpus.resolve()
runtime = P.Path(tempfile.mkdtemp(prefix='fxi-startup-comparison-'))
env = {**os.environ, 'FXI_INDEXES': str(args.indexes.resolve()),
       'FXI_SOCKET': str(runtime / 'unused.sock'), 'XDG_RUNTIME_DIR': str(runtime)}
binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
if args.tgrep:
    binaries['tgrep'] = args.tgrep.resolve()

def run(command):
    start = time.perf_counter_ns()
    result = sp.run(command, cwd=root, env=env, capture_output=True, timeout=120)
    elapsed = (time.perf_counter_ns() - start) / 1e6
    if result.returncode not in (0, 1) or b'Daemon search failed' in result.stderr:
        raise RuntimeError((command, result.returncode, result.stderr.decode()))
    paths = []
    for line in result.stdout.decode().splitlines():
        path, count = line.rsplit(':', 1) if args.count else (line, None)
        path = str(P.Path(path).relative_to(root)) if P.Path(path).is_absolute() else path.removeprefix('./')
        paths.append((path, int(count)) if count is not None else path)
    assert len(paths) == len(set(paths)), 'Duplicate file records'
    return elapsed, set(paths)

rows = []
queries = [(p, p) for p in args.patterns] if args.patterns else [('absent', 'auditNonexistentSymbol94283'), ('selective', 'folio_wait_bit_common')]
output_flag = '-c' if args.count else '-l'
other_flags = ['-i', '-F'] if args.literal else []
for label, pattern in queries:
    expected = run(['rg', *other_flags, output_flag, '--color=never', pattern, '.'])[1]
    fxi_pattern = pattern if args.literal else f're:/{pattern}/'
    commands = {name: ([str(binary), *other_flags, output_flag, '--color=never', pattern, str(root)] if name == 'tgrep' else
                      [str(binary), output_flag, '--color=never', fxi_pattern, '-p', str(root)])
                for name, binary in binaries.items()}
    for command in commands.values():
        assert run(command)[1] == expected
    samples = {name: [] for name in commands}
    for rep in range(args.repetitions):
        order = list(commands)
        random.Random(1729 + rep).shuffle(order)
        for name in order:
            elapsed, paths = run(commands[name])
            assert paths == expected, (label, name, paths ^ expected)
            samples[name].append(elapsed)
    row = {'query': label, 'pattern': pattern, 'files': len(expected),
           'tools': {name: {'median_ms': statistics.median(values), 'samples_ms': values}
                     for name, values in samples.items()}}
    rows.append(row)
    print(label, {name: data['median_ms'] for name, data in row['tools'].items()}, flush=True)
result = {'corpus': str(root), 'indexes': str(args.indexes.resolve()), 'mode': 'direct', 'output_mode': 'count' if args.count else 'files', 'query_syntax': 'plain' if args.literal else 'regex',
          'binaries': {name: {'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
                       for name, binary in binaries.items()}, 'rows': rows}
args.output.write_text(json.dumps(result, indent=2))
