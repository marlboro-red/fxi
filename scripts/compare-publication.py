"""Measure one-file durable updates after different inherited segment counts.

Each timed update starts from a private copy of the same fragmented generation.
Only CLI update execution is timed. Exact new/old file membership and a reopened
reader are checked afterward. This measures durable publication, not watcher lag.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess as sp
import tempfile
import time


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument('--baseline', type=Path, required=True)
    p.add_argument('--candidate', type=Path, required=True)
    p.add_argument('--candidate-query-local', action='store_true', help='Prepare checked evidence and enable candidate query-local validation')
    p.add_argument('--baseline-query-local', action='store_true')
    p.add_argument('--candidate-generation-routing', action='store_true')
    p.add_argument('--repetitions', type=int, default=5)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    if args.repetitions < 1:
        p.error('repetitions must be positive')
    binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
    base = Path(tempfile.mkdtemp(prefix='fxi-publication-comparison-'))
    root = base / 'corpus'
    root.mkdir()
    for i in range(4096):
        (root / f'{i:04}.rs').write_text(f'fn symbol_{i}() {{}}\n' + 'shared original evidence\n' * 20)
    sp.run(['git', 'init', '-q', str(root)], check=True)
    env = {**os.environ, 'FXI_SOCKET': str(base / 'unused.sock'), 'XDG_RUNTIME_DIR': str(base),
           'FXI_SOURCE_PACK': '0', 'FXI_TRACE_UPDATES': '1'}
    def run(command, indexes):
        return sp.run([str(x) for x in command], cwd=root, env={**env, 'FXI_INDEXES': str(indexes), 'FXI_QUERY_LOCAL': '1' if (args.candidate_query_local if command[0] == binaries['after'] else args.baseline_query_local) else '0', 'FXI_GENERATION_ROUTING': '1' if args.candidate_generation_routing and command[0] == binaries['after'] else '0'},
                      capture_output=True, text=True, check=True, timeout=120)
    result = {'base': str(base), 'files': 4096, 'profile': 'full', 'source_pack': False, 'candidate_query_local': args.candidate_query_local, 'baseline_query_local': args.baseline_query_local, 'candidate_generation_routing': args.candidate_generation_routing,
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'binaries': {v: {'path': str(b), 'sha256': hashlib.sha256(b.read_bytes()).hexdigest()} for v, b in binaries.items()},
              'rows': []}
    for segments in [1, 64, 256]:
        added = root / 'new.rs'
        added.unlink(missing_ok=True)
        prepared = base / f'prepared-{segments}'
        run([binaries['after'] if args.candidate_query_local else binaries['before'], 'index', '--force', '--chunk-size', 4096 // segments, root], prepared)
        added.write_text('newPublicationMarker shared\n')
        samples = {'before': [], 'after': []}
        for repetition in range(args.repetitions):
            for variant in (['before', 'after'] if repetition % 2 == 0 else ['after', 'before']):
                indexes = base / f'{segments}-{repetition}-{variant}'
                shutil.copytree(prepared, indexes)
                start = time.perf_counter()
                proc = run([binaries[variant], 'index', root], indexes)
                elapsed = time.perf_counter() - start
                for pattern, expected in [('newPublicationMarker', {'new.rs'}), ('shared', {f'{i:04}.rs' for i in range(4096)} | {'new.rs'})]:
                    output = run([binaries[variant], '-l', '--color=never', f're:/{pattern}/', '-p', root], indexes).stdout
                    paths = [Path(line).name for line in output.splitlines()]
                    assert len(paths) == len(set(paths)) and set(paths) == expected
                current, = indexes.rglob('CURRENT')
                generation = current.parent / 'generations' / current.read_text().strip()
                meta = json.loads((generation / 'meta.json').read_text())
                assert meta['doc_count'] == 4097 and meta['segment_count'] == segments + 1
                samples[variant].append({'seconds': elapsed, 'stdout': proc.stdout, 'stderr': proc.stderr})
                shutil.rmtree(indexes)
        row = {'inherited_segments': segments, 'samples': samples,
               'median_seconds': {v: statistics.median(s['seconds'] for s in values) for v, values in samples.items()}}
        result['rows'].append(row)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')
        print(segments, row['median_seconds'], flush=True)
        shutil.rmtree(prepared)
    shutil.rmtree(root)


if __name__ == '__main__':
    main()
