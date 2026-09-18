"""Compare full index builds with isolated output directories and exact search checks.

macOS /usr/bin/time -l supplies peak RSS. Existing indexes are never changed.
Only the last build of each variant is retained for subsequent search benchmarks.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess as sp
import tempfile
import time


def corpus_manifest(root):
    paths = sp.check_output(['rg', '--files', '-0'], cwd=root).decode().split('\0')
    paths = sorted(path for path in paths if path)
    digest = hashlib.sha256()
    size = 0
    for name in paths:
        data = (root / name).read_bytes()
        size += len(data)
        digest.update(json.dumps([name, hashlib.sha256(data).hexdigest()],
                                 separators=(',', ':')).encode() + b'\n')
    return {'files': len(paths), 'source_bytes': size, 'sha256': digest.hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--corpus', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--baseline-profile', choices=['full', 'lean'])
    parser.add_argument('--candidate-profile', choices=['full', 'lean'])
    parser.add_argument('--candidate-query-local', action='store_true', help='Build candidate checked gram evidence and enable query-local validation')
    parser.add_argument('--baseline-query-local', action='store_true')
    parser.add_argument('--candidate-generation-routing', action='store_true')
    parser.add_argument('--source-pack', choices=['0', '1'], default='0')
    parser.add_argument('--repetitions', type=int, default=3)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error('repetitions must be positive')
    root = args.corpus.resolve(strict=True)
    manifest = corpus_manifest(root)
    binaries = {name: path.resolve(strict=True) for name, path in
                [('before', args.baseline), ('after', args.candidate)]}
    runtime = Path(tempfile.mkdtemp(prefix='fxi-build-comparison-'))
    base_env = {**os.environ, 'FXI_SOURCE_PACK': args.source_pack,
                'FXI_SOCKET': str(runtime / 'unused.sock'),
                'XDG_RUNTIME_DIR': str(runtime)}
    patterns = ['auditNonexistentSymbol94283', 'folio_wait_bit_common',
                'struct file_operations', 'return']
    expected = {}
    for pattern in patterns:
        result = sp.run(['rg', '-l', '-0', '-F', '--', pattern, '.'], cwd=root,
                        capture_output=True, timeout=120)
        if result.returncode not in (0, 1):
            raise RuntimeError(result.stderr.decode())
        expected[pattern] = {p.removeprefix('./') for p in
                             result.stdout.decode().split('\0') if p}
    samples = {name: [] for name in binaries}
    retained = {}
    for repetition in range(args.repetitions):
        for name in (['before', 'after'] if repetition % 2 == 0 else ['after', 'before']):
            indexes = runtime / f'{repetition}-{name}'
            indexes.mkdir()
            env = {**base_env, 'FXI_INDEXES': str(indexes), 'FXI_QUERY_LOCAL': '1' if (args.candidate_query_local if name == 'after' else args.baseline_query_local) else '0', 'FXI_GENERATION_ROUTING': '1' if name == 'after' and args.candidate_generation_routing else '0'}
            command = ['/usr/bin/time', '-l', str(binaries[name]), 'index', str(root), '--force']
            profile = args.baseline_profile if name == 'before' else args.candidate_profile
            if profile is not None:
                command.extend(['--profile', profile])
            started = time.perf_counter_ns()
            result = sp.run(command, env=env, cwd=root, capture_output=True, timeout=600)
            elapsed = (time.perf_counter_ns() - started) / 1e9
            if result.returncode:
                raise RuntimeError(result.stderr.decode())
            stderr = result.stderr.decode()
            match = re.search(r'(\d+)\s+maximum resident set size', stderr)
            if match is None:
                raise RuntimeError('This harness requires macOS /usr/bin/time -l RSS output')
            components = {}
            for path in indexes.rglob('*'):
                if path.is_file():
                    components[path.name] = components.get(path.name, 0) + path.stat().st_size
            # Correctness checks are outside the build timing and before cleanup.
            for pattern in patterns:
                check = sp.run([str(binaries[name]), '-l', '-0', '-F', '--', pattern, str(root)],
                               env=env, cwd=root, capture_output=True, timeout=120)
                if check.returncode or b'Daemon search failed' in check.stderr:
                    raise RuntimeError(check.stderr.decode())
                paths = [p for p in check.stdout.decode().split('\0') if p]
                if len(paths) != len(set(paths)) or set(paths) != expected[pattern]:
                    raise AssertionError((name, pattern, set(paths) ^ expected[pattern]))
            samples[name].append({'seconds': elapsed, 'peak_rss_bytes': int(match.group(1)),
                                  'index_bytes': sum(components.values()),
                                  'components': components, 'command': command})
            if name in retained:
                # This path was created by this invocation, never a supplied index.
                shutil.rmtree(retained[name])
            retained[name] = indexes
            print(repetition, name, round(elapsed, 3), 'seconds',
                  sum(components.values()), 'bytes', flush=True)
    assert corpus_manifest(root) == manifest, 'Corpus changed during comparison'
    result = {'corpus': str(root), 'corpus_manifest': manifest,
              'runtime': str(runtime), 'source_pack': args.source_pack,
              'candidate_query_local': args.candidate_query_local, 'baseline_query_local': args.baseline_query_local, 'candidate_generation_routing': args.candidate_generation_routing,
              'mode': 'full builds, warm filesystem; oracle checks outside timing',
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'retained_indexes': {name: str(path) for name, path in retained.items()},
              'oracle_file_counts': {p: len(paths) for p, paths in expected.items()},
              'binaries': {name: {'path': str(binary),
                                  'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
                           for name, binary in binaries.items()},
              'tools': {name: {'samples': values,
                               'median_seconds': statistics.median(v['seconds'] for v in values),
                               'median_peak_rss_bytes': statistics.median(v['peak_rss_bytes'] for v in values)}
                        for name, values in samples.items()}}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
