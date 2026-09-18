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
parser.add_argument('--candidate-indexes', type=P.Path, help='Compare another segment layout with the same corpus')
parser.add_argument('--baseline', required=True, type=P.Path)
parser.add_argument('--candidate', required=True, type=P.Path)
parser.add_argument('--tgrep', type=P.Path)
parser.add_argument('--repetitions', type=int, default=31)
parser.add_argument('--output', required=True, type=P.Path)
parser.add_argument('--baseline-query-local', action='store_true', help='Enable checked query-local validation for baseline')
parser.add_argument('--candidate-query-local', action='store_true', help='Enable checked query-local posting validation for candidate only')
parser.add_argument('--count', action='store_true')
parser.add_argument('--literal', action='store_true', help='Compare plain FXI identifier queries with case-insensitive fixed strings')
parser.add_argument('--patterns', nargs='+')
parser.add_argument('--before-threads', type=int)
parser.add_argument('--after-threads', type=int)
parser.add_argument('--before-search-parallelism', type=int)
parser.add_argument('--after-search-parallelism', type=int)
args = parser.parse_args()
if args.repetitions < 1:
    parser.error('repetitions must be positive')
if any(value is not None and value < 1 for value in [args.before_threads, args.after_threads,
       args.before_search_parallelism, args.after_search_parallelism]):
    parser.error('Thread counts must be positive')
threads = {'before': args.before_threads, 'after': args.after_threads}
search_parallelism = {'before': args.before_search_parallelism, 'after': args.after_search_parallelism}
root = args.corpus.resolve()
runtime = P.Path(tempfile.mkdtemp(prefix='fxi-startup-comparison-'))
env = {**os.environ, 'FXI_INDEXES': str(args.indexes.resolve()),
       'FXI_SOCKET': str(runtime / 'unused.sock'), 'XDG_RUNTIME_DIR': str(runtime)}
binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
if args.tgrep:
    binaries['tgrep'] = args.tgrep.resolve()

def run(command, thread_count=None, indexes=None, search_tasks=None, query_local=False):
    start = time.perf_counter_ns()
    child_env = dict(env, FXI_QUERY_LOCAL='1' if query_local else '0')
    if thread_count is not None:
        child_env['RAYON_NUM_THREADS'] = str(thread_count)
    if indexes is not None:
        child_env['FXI_INDEXES'] = str(indexes.resolve())
    if search_tasks is not None:
        child_env['FXI_SEARCH_PARALLELISM'] = str(search_tasks)
    result = sp.run(command, cwd=root, env=child_env, capture_output=True, timeout=120)
    elapsed = (time.perf_counter_ns() - start) / 1e6
    valid_codes = (0,) if command[0] in {str(binaries['before']), str(binaries['after'])} else (0, 1)
    if result.returncode not in valid_codes or b'Daemon search failed' in result.stderr:
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
    for name, command in commands.items():
        assert run(command, threads.get(name), args.candidate_indexes if name == 'after' else None,
                   search_parallelism.get(name), (args.candidate_query_local if name == 'after' else args.baseline_query_local) if name != 'tgrep' else False)[1] == expected
    samples = {name: [] for name in commands}
    for rep in range(args.repetitions):
        order = list(commands)
        random.Random(1729 + rep).shuffle(order)
        for name in order:
            elapsed, paths = run(commands[name], threads.get(name), args.candidate_indexes if name == 'after' else None,
                                 search_parallelism.get(name), (args.candidate_query_local if name == 'after' else args.baseline_query_local) if name != 'tgrep' else False)
            assert paths == expected, (label, name, paths ^ expected)
            samples[name].append(elapsed)
    row = {'query': label, 'pattern': pattern, 'files': len(expected),
           'tools': {name: {'median_ms': statistics.median(values), 'samples_ms': values}
                     for name, values in samples.items()}}
    rows.append(row)
    print(label, {name: data['median_ms'] for name, data in row['tools'].items()}, flush=True)
result = {'harness_sha256': hashlib.sha256(P.Path(__file__).read_bytes()).hexdigest(), 'candidate_indexes': str(args.candidate_indexes.resolve()) if args.candidate_indexes else None, 'rayon_threads': threads, 'search_parallelism': search_parallelism, 'corpus': str(root), 'indexes': str(args.indexes.resolve()), 'mode': 'direct', 'output_mode': 'count' if args.count else 'files', 'query_syntax': 'plain' if args.literal else 'regex', 'candidate_query_local': args.candidate_query_local, 'baseline_query_local': args.baseline_query_local,
          'binaries': {name: {'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
                       for name, binary in binaries.items()}, 'rows': rows}
args.output.write_text(json.dumps(result, indent=2))
