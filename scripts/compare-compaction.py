"""Compare compaction on byte-identical fragmented indexes, with exact query checks.

Preparation/copying/verification are outside the measured region. Each variant
gets a private copy; the corpus is read-only. Reports retain binary hashes, raw
resource output and component hashes, not just summary timings.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess as sp
import tempfile
import time


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument('--corpus', type=Path, required=True)
    p.add_argument('--baseline', type=Path, required=True)
    p.add_argument('--candidate', type=Path, required=True)
    p.add_argument('--profile', choices=['lean', 'full'], required=True)
    p.add_argument('--chunk-size', type=int, default=4096)
    p.add_argument('--repetitions', type=int, default=3)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    if min(args.chunk_size, args.repetitions) < 1:
        p.error('chunk size and repetitions must be positive')
    root = args.corpus.resolve()
    binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
    base = Path(tempfile.mkdtemp(prefix='fxi-compaction-comparison-'))
    env = {**os.environ, 'FXI_SOCKET': str(base / 'unused.sock'),
           'XDG_RUNTIME_DIR': str(base), 'FXI_SOURCE_PACK': '0'}
    names = sorted(n for n in sp.check_output(['rg', '--files', '-0'], cwd=root).decode().split('\0') if n)
    def manifest():
        h = hashlib.sha256()
        for name in names:
            h.update(json.dumps([name, hashlib.sha256((root / name).read_bytes()).hexdigest()], separators=(',', ':')).encode() + b'\n')
        return h.hexdigest()
    initial_manifest = manifest()
    def run(command, indexes, **kwargs):
        return sp.run([str(x) for x in command], env={**env, 'FXI_INDEXES': str(indexes)}, cwd=root,
                      capture_output=True, text=True, check=True, timeout=600, **kwargs)
    prepared = base / 'prepared'
    run([binaries['before'], 'index', '--force', '--profile', args.profile, '--chunk-size', args.chunk_size, root], prepared)
    patterns = ['folio_wait_bit_common', 'struct file_operations', 'return.*0', 'auditNonexistentSymbol94283']
    def paths(stdout):
        result = [str(Path(line).relative_to(root)) if Path(line).is_absolute() else line.removeprefix('./') for line in stdout.splitlines()]
        assert len(result) == len(set(result)), 'duplicate paths'
        return set(result)
    expected = {}
    for pattern in patterns:
        proc = sp.run(['rg', '-l', '--color=never', pattern, '.'], cwd=root, capture_output=True, text=True)
        assert proc.returncode in (0, 1), proc.stderr
        expected[pattern] = paths(proc.stdout)
    result = {'corpus': str(root), 'manifest_sha256': initial_manifest, 'files': len(names),
              'profile': args.profile, 'chunk_size': args.chunk_size, 'source_pack': False,
              'platform': platform.platform(), 'base': str(base),
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'binaries': {v: {'path': str(b), 'sha256': hashlib.sha256(b.read_bytes()).hexdigest()} for v, b in binaries.items()},
              'samples': []}
    reference = None
    for repetition in range(args.repetitions):
        for variant in (['before', 'after'] if repetition % 2 == 0 else ['after', 'before']):
            indexes = base / f'{repetition}-{variant}'
            shutil.copytree(prepared, indexes)
            flags = ['-l'] if platform.system() == 'Darwin' else ['-v']
            start = time.perf_counter()
            proc = run(['/usr/bin/time', *flags, binaries[variant], 'compact', root], indexes)
            elapsed = time.perf_counter() - start
            if platform.system() == 'Darwin':
                rss = int(re.search(r'(\d+)\s+maximum resident set size', proc.stderr).group(1))
            else:
                rss = 1024 * int(re.search(r'Maximum resident set size \(kbytes\): (\d+)', proc.stderr).group(1))
            for pattern in patterns:
                actual = paths(run([binaries[variant], '-l', '--color=never', f're:/{pattern}/', '-p', root], indexes).stdout)
                assert actual == expected[pattern], (variant, pattern, actual ^ expected[pattern])
            current, = indexes.rglob('CURRENT')
            generation = current.parent / 'generations' / current.read_text().strip()
            meta = json.loads((generation / 'meta.json').read_text())
            assert meta['doc_count'] == len(names) and meta['segment_count'] == 1
            segment = generation / 'segments' / 'seg_0001'
            hashes = {f.name: hashlib.sha256(f.read_bytes()).hexdigest() for f in segment.iterdir() if f.is_file() and f.name != 'linemap.bin'}
            if reference is None:
                reference = hashes
            assert hashes == reference, 'compacted posting/position components differ'
            sample = {'variant': variant, 'repetition': repetition, 'seconds': elapsed, 'max_rss_bytes': rss,
                      'resource_output': proc.stderr, 'component_sha256': hashes,
                      'index_bytes': sum(f.stat().st_size for f in generation.rglob('*') if f.is_file())}
            result['samples'].append(sample)
            print(variant, repetition, round(elapsed, 3), rss, flush=True)
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(result, indent=2) + '\n')
            shutil.rmtree(indexes)
    assert manifest() == initial_manifest, 'corpus changed'
    result['summary'] = {v: {k: statistics.median(s[k] for s in result['samples'] if s['variant'] == v)
                            for k in ['seconds', 'max_rss_bytes', 'index_bytes']} for v in binaries}
    result['manifest_verified_after'] = True
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    shutil.rmtree(prepared)


if __name__ == '__main__':
    main()
