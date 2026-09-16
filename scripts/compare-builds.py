"""Interleave old/new FXI binaries on one prepared corpus; optionally add tgrep.

Example: python3 scripts/compare-builds.py --corpus /tmp/fxi-linux-corpus \
  --variant before=/tmp/fxi-round1-baseline --variant after=target/release/fxi \
  --tgrep /tmp/fxi-audit-tgrep/target/release/tgrep --output comparison.json
Only build commands are timed. Every resulting index must list every corpus file.
"""
import argparse
import hashlib
import json
import os
import pathlib as P
import random
import re
import statistics
import subprocess as sp
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--corpus', required=True, type=P.Path)
parser.add_argument('--variant', action='append', required=True, help='NAME=BINARY')
parser.add_argument('--tgrep', type=P.Path)
parser.add_argument('--repetitions', type=int, default=5)
parser.add_argument('--output', required=True, type=P.Path)
args = parser.parse_args()
root = args.corpus.resolve()
base = P.Path(tempfile.mkdtemp(prefix='fxi-build-comparison-'))
variants = {}
for spec in args.variant:
    name, sep, binary = spec.partition('=')
    if not sep or not re.fullmatch(r'[A-Za-z0-9_-]+', name) or name in variants:
        parser.error('Variants need distinct NAME=BINARY entries')
    variants[name] = {'binary': str(P.Path(binary).resolve()), 'kind': 'fxi'}
if args.tgrep:
    if 'tgrep' in variants:
        parser.error('tgrep is reserved for --tgrep')
    variants['tgrep'] = {'binary': str(args.tgrep.resolve()), 'kind': 'tgrep'}
if args.repetitions < 1:
    parser.error('repetitions must be positive')
names = sp.check_output(['rg', '--files', '-0'], cwd=root).decode().split('\0')
names = sorted(name for name in names if name)
manifest = [(name, hashlib.sha256((root / name).read_bytes()).hexdigest()) for name in names]
for name, variant in variants.items():
    runtime = base / name
    runtime.mkdir()
    variant['env'] = {**os.environ, 'FXI_INDEXES': str(runtime / 'indexes'),
                      'FXI_SOCKET': str(runtime / 'fxi.sock'), 'XDG_RUNTIME_DIR': str(runtime)}
    variant['binary_sha256'] = hashlib.sha256(P.Path(variant['binary']).read_bytes()).hexdigest()
    variant['samples'] = []
for rep in range(args.repetitions):
    order = list(variants)
    random.Random(9137 + rep).shuffle(order)
    for name in order:
        variant = variants[name]
        command = [variant['binary'], 'index', '--force', str(root)]
        start = time.perf_counter()
        proc = sp.run(['/usr/bin/time', '-l', *command], cwd=root, env=variant['env'],
                      capture_output=True, text=True, check=True, timeout=600)
        seconds = time.perf_counter() - start
        sample = {'seconds': seconds, 'max_rss_bytes': int(re.search(
            r'(\d+)\s+maximum resident set size', proc.stderr).group(1)), 'resource_output': proc.stderr}
        # Outside the timed region: every rebuilt index must enumerate the
        # complete prepared corpus, not just a few hand-picked positive hits.
        command = ([variant['binary'], '-l', '--color=never', 're:/^/', '-p', str(root)]
                   if variant['kind'] == 'fxi' else
                   [variant['binary'], '-l', '--color=never', '^', str(root)])
        found = sp.run(command, cwd=root, env=variant['env'], capture_output=True,
                       text=True, check=True, timeout=120)
        paths = {str(P.Path(p).relative_to(root)) if P.Path(p).is_absolute() else p.removeprefix('./')
                 for p in found.stdout.splitlines()}
        expected = set(names)
        if paths != expected:
            raise RuntimeError({'variant': name, 'missing': sorted(expected-paths)[:20],
                                'extra': sorted(paths-expected)[:20]})
        sample['validated_files'] = len(paths)
        variant['samples'].append(sample)
        print(rep, name, round(seconds, 3), round(sample['max_rss_bytes']/2**20, 1), flush=True)
for name, variant in variants.items():
    index_root = root / '.tgrep' if variant['kind'] == 'tgrep' else base / name / 'indexes'
    variant['index_bytes'] = sum(p.stat().st_size for p in index_root.rglob('*') if p.is_file())
    variant.pop('env')
    variant['median_seconds'] = statistics.median(s['seconds'] for s in variant['samples'])
    variant['median_peak_rss_bytes'] = statistics.median(s['max_rss_bytes'] for s in variant['samples'])
result = {'corpus': str(root), 'base': str(base), 'files': len(names),
          'manifest_sha256': hashlib.sha256(json.dumps(manifest).encode()).hexdigest(), 'variants': variants}
args.output.write_text(json.dumps(result, indent=2))
