"""Separate read-mode effects using the Chromium benchmark's existing indexes."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import random
import statistics
import subprocess as sp
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--snapshot', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--checked-indexes', type=Path, required=True,
                        help='Full-profile index built with query-local validation and routing enabled')
    parser.add_argument('--repetitions', type=int, default=7)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error('repetitions must be positive')
    snapshot = json.loads(args.snapshot.read_text())
    if not snapshot.get('completed'):
        parser.error('Complete the primary benchmark before running this comparison')
    root, base = Path(snapshot['corpus']), Path(snapshot['base'])
    binary = snapshot['binaries']['fxi']['path']
    assert hashlib.sha256(Path(binary).read_bytes()).hexdigest() == snapshot['binaries']['fxi']['sha256']
    helper_path = Path(__file__).with_name('benchmark-chromium.py')
    spec = importlib.util.spec_from_file_location('chromium', helper_path)
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith('FXI_') and key not in ('RIPGREP_CONFIG_PATH', 'RAYON_NUM_THREADS')}
    env.update(FXI_APP_DATA=str(base / 'ablation-app'), FXI_SOCKET=str(base / 'unused-ablation.sock'),
               XDG_RUNTIME_DIR=str(base))
    # Each tuple specifies index, query-local validation, routing, and packs.
    modes = {'full-default': ('default', 0, 0, 0),
             'full-query-local': ('checked', 1, 0, 0),
             'full-checked-routed': ('checked', 1, 1, 0),
             'lean-stable-live': ('opt', 1, 1, 0),
             'lean-stable-packed': ('opt', 1, 1, 1)}
    result = {'snapshot': str(args.snapshot), 'revision': snapshot['revision'],
              'fxi_revision': snapshot['fxi_revision'], 'binary': snapshot['binaries']['fxi'],
              'mode': 'fresh CLI; warm filesystem; existing immutable indexes; no new builds',
              'modes': modes, 'repetitions': args.repetitions,
              'checked_indexes': str(args.checked_indexes.resolve()),
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'helper_sha256': hashlib.sha256(helper_path.read_bytes()).hexdigest(), 'rows': []}
    for name in ('rare', 'absent', 'namespace', 'punctuation'):
        workload = next(row for row in snapshot['rows'] if row['name'] == name)
        pattern = workload['pattern']
        oracle = sp.run([snapshot['binaries']['ripgrep']['path'], '-l', '-F', '--', pattern, '.'],
                        cwd=root, env=env, capture_output=True, timeout=300)
        assert oracle.returncode in (0, 1), oracle.stderr
        expected = helper.paths(oracle.stdout, root)
        assert len(expected) == workload['oracle_files']
        samples = {mode: [] for mode in modes}
        for repetition in range(-1, args.repetitions):
            order = list(modes)
            random.Random(17023 + repetition).shuffle(order)
            for mode in order:
                index, checked, routing, packed = modes[mode]
                index_path = args.checked_indexes.resolve() if index == 'checked' else base / index
                child = dict(env, FXI_INDEXES=str(index_path),
                    FXI_QUERY_LOCAL=str(checked), FXI_GENERATION_ROUTING=str(routing),
                    FXI_SOURCE_PACK=str(packed), FXI_SOURCE_PACK_COMPRESSION=str(packed),
                    FXI_STABLE_SEGMENTS=str(int(index == 'opt')))
                start = time.perf_counter_ns()
                proc = sp.run([binary, '-l', '--color=never', '-F', '-e', pattern, '-p', str(root)],
                              cwd=root, env=child, capture_output=True, timeout=300)
                elapsed = (time.perf_counter_ns() - start) / 1e6
                assert proc.returncode == 0 and b'Daemon search failed' not in proc.stderr, proc.stderr
                assert helper.paths(proc.stdout, root) == expected, (mode, pattern)
                if repetition >= 0:
                    samples[mode].append(elapsed)
        row = {'name': name, 'pattern': pattern, 'files': len(expected), 'all_results_exact': True,
               'modes': {mode: {'samples_ms': values, 'median_ms': statistics.median(values)}
                         for mode, values in samples.items()}}
        result['rows'].append(row)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')
        print(name, {mode: round(values['median_ms'], 2) for mode, values in row['modes'].items()}, flush=True)
    assert not sp.check_output(['git', 'status', '--porcelain', '--untracked-files=no'], cwd=root)


if __name__ == '__main__':
    main()
