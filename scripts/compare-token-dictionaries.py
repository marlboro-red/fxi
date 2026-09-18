"""Interleave eager-reader/token API probes; compare path fingerprints per query."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess as sp


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--corpus', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--indexes', type=Path, required=True)
    parser.add_argument('--candidate-indexes', type=Path, required=True)
    parser.add_argument('--repetitions', type=int, default=7)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error('repetitions must be positive')
    binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
    indexes = {'before': args.indexes.resolve(), 'after': args.candidate_indexes.resolve()}
    samples = {name: [] for name in binaries}
    expected = None
    for repetition in range(args.repetitions):
        for name in (['before', 'after'] if repetition % 2 == 0 else ['after', 'before']):
            result = sp.run(['/usr/bin/time', '-l', str(binaries[name]), str(args.corpus.resolve())],
                            env={**os.environ, 'FXI_INDEXES': str(indexes[name])},
                            capture_output=True, text=True, check=True, timeout=120)
            rss = re.search(r'(\d+)\s+maximum resident set size', result.stderr)
            assert rss is not None
            sample = {'peak_rss_bytes': int(rss.group(1)), 'queries': {}}
            fingerprints = {}
            for line in result.stdout.splitlines():
                fields = line.split('\t')
                if fields[0] == 'open_ms':
                    sample['open_ms'] = float(fields[1])
                elif fields[0] == 'query':
                    _, contains, token, count, fingerprint, elapsed = fields
                    key = f'{contains}:{token}'
                    fingerprints[key] = [int(count), fingerprint]
                    sample['queries'][key] = float(elapsed)
                else:
                    raise ValueError(line)
            assert 'open_ms' in sample and len(fingerprints) == 8
            if expected is None:
                expected = fingerprints
            assert fingerprints == expected, (name, fingerprints, expected)
            samples[name].append(sample)
            print(repetition, name, round(sample['open_ms'], 3), 'ms', flush=True)
    output = {'corpus': str(args.corpus.resolve()),
              'indexes': {k: str(v) for k, v in indexes.items()},
              'binaries': {k: {'path': str(v), 'sha256': hashlib.sha256(v.read_bytes()).hexdigest()}
                           for k, v in binaries.items()},
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'path_fingerprints': expected,
              'limits': 'Warm filesystem; sorted path counts + 64-bit fingerprints, not independent token oracle. Query samples are medians of11 warm API calls per process. Eager open and peak processRSS include token validation; excludes CLI/daemon transport.',
              'tools': {name: {'samples': values,
                               'median_open_ms': statistics.median(v['open_ms'] for v in values),
                               'median_peak_rss_bytes': statistics.median(v['peak_rss_bytes'] for v in values),
                               'median_query_ms': {q: statistics.median(v['queries'][q] for v in values)
                                                   for q in expected}}
                        for name, values in samples.items()}}
    args.output.write_text(json.dumps(output, indent=2) + '\n')


if __name__ == '__main__':
    main()
