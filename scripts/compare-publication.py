"""Measure one-file durable updates after different inherited segment counts.

Each timed update starts from a private copy of the same fragmented generation.
Only CLI update execution is timed. Exact new/old file membership and a reopened
reader are checked afterward. This measures durable publication, not watcher lag.
"""
import argparse
import hashlib
import importlib.util
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
    p.add_argument('--baseline-generation-routing', action='store_true')
    p.add_argument('--candidate-stable-segments', action='store_true', help='Prepare and update candidate using experimental stable segment objects')
    p.add_argument('--source-pack', action='store_true', help='Enable compressed source packs for both layouts')
    p.add_argument('--corpus', type=Path, help='Copy an existing corpus for a realistic one-file update; never modifies the supplied source')
    p.add_argument('--chunk-size', type=int, default=2048, help='Initial chunk size for --corpus')
    p.add_argument('--keep-fixture', action='store_true', help='Retain corpus and prepared indexes for separate query controls')
    p.add_argument('--repetitions', type=int, default=5)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    if args.repetitions < 1 or args.chunk_size < 1:
        p.error('repetitions and chunk-size must be positive')
    policies = {'before': (args.baseline_query_local, args.baseline_generation_routing),
                'after': (args.candidate_query_local, args.candidate_generation_routing)}
    for variant, (query_local, generation_routing) in policies.items():
        if generation_routing and not query_local:
            p.error(f'{variant} generation routing requires query-local validation')
    binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
    base = Path(tempfile.mkdtemp(prefix='fxi-publication-comparison-'))
    root = base / 'corpus'
    if args.corpus:
        # Copy bytes, not hard links: links would change source ctimes and
        # invalidate other experiments' packed-source freshness evidence.
        shutil.copytree(args.corpus.resolve(), root, ignore=shutil.ignore_patterns('.git'))
    else:
        root.mkdir()
        for i in range(4096):
            (root / f'{i:04}.rs').write_text(f'fn symbol_{i}() {{}}\n' + 'shared original evidence\n' * 20)
    sp.run(['git', 'init', '-q', str(root)], check=True)
    manifest_helper = Path(__file__).with_name('compare-index-builds.py')
    spec = importlib.util.spec_from_file_location('index_build_manifest', manifest_helper)
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    manifest = helper.corpus_manifest(root)
    env = {**os.environ, 'FXI_SOCKET': str(base / 'unused.sock'), 'XDG_RUNTIME_DIR': str(base),
           'FXI_SOURCE_PACK': '1' if args.source_pack else '0', 'FXI_SOURCE_PACK_COMPRESSION': '1' if args.source_pack else '0', 'FXI_TRACE_UPDATES': '1', 'FXI_APP_DATA': str(base / 'app-data')}
    def run(command, indexes, variant):
        query_local, generation_routing = policies[variant]
        return sp.run([str(x) for x in command], cwd=root, env={**env, 'FXI_INDEXES': str(indexes), 'FXI_QUERY_LOCAL': '1' if query_local else '0', 'FXI_GENERATION_ROUTING': '1' if generation_routing else '0', 'FXI_STABLE_SEGMENTS': '1' if variant == 'after' and args.candidate_stable_segments else '0', 'FXI_NEGATIVE_ROUTING': '0'},
                      capture_output=True, text=True, check=True, timeout=120)
    result = {'candidate_stable_segments': args.candidate_stable_segments, 'base': str(base), 'source_corpus': str(args.corpus.resolve()) if args.corpus else None, 'files': None if args.corpus else 4096, 'profile': 'lean' if args.corpus else 'full', 'source_pack': args.source_pack, 'candidate_query_local': args.candidate_query_local, 'baseline_query_local': args.baseline_query_local, 'candidate_generation_routing': args.candidate_generation_routing, 'baseline_generation_routing': args.baseline_generation_routing,
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'manifest_helper_sha256': hashlib.sha256(manifest_helper.read_bytes()).hexdigest(), 'corpus_manifest': manifest,
              'binaries': {v: {'path': str(b), 'sha256': hashlib.sha256(b.read_bytes()).hexdigest()} for v, b in binaries.items()},
              'rows': [], 'retained_fixtures': []}
    for chunk_size in ([args.chunk_size] if args.corpus else [4096, 64, 16]):
        added = root / '__fxi_publication_probe_94283.rs'
        if args.corpus:
            assert not added.exists(), 'Probe filename already exists in supplied corpus'
        else:
            added.unlink(missing_ok=True)
        prepared = base / f'prepared-{chunk_size}'
        # Each binary prepares its own policy/layout; never let the candidate's
        # stable flag accidentally turn the legacy control into a stable index.
        run([binaries['before'], 'index', '--force', '--profile', result['profile'], '--chunk-size', chunk_size, root], prepared, 'before')
        candidate_prepared = base / f'prepared-candidate-{chunk_size}'
        run([binaries['after'], 'index', '--force', '--profile', result['profile'], '--chunk-size', chunk_size, root], candidate_prepared, 'after')
        current, = prepared.rglob('CURRENT')
        generation = current.parent / 'generations' / current.read_text().strip()
        original = json.loads((generation / 'meta.json').read_text())
        segments = original['segment_count']
        files = original['doc_count']
        candidate_current, = candidate_prepared.rglob('CURRENT')
        candidate_generation = candidate_current.parent / 'generations' / candidate_current.read_text().strip()
        candidate_meta = json.loads((candidate_generation / 'meta.json').read_text())
        legacy_version = 3 if result['profile'] == 'lean' else 2
        assert original['version'] == legacy_version, 'Baseline must use the legacy layout'
        assert candidate_meta['version'] == (legacy_version + 2 if args.candidate_stable_segments else legacy_version)
        assert candidate_meta['segment_count'] == segments and candidate_meta['doc_count'] == files
        assert files >= 16, 'Use at least 16 indexed files so adding one does not trigger a full rebuild'
        result['files'] = files
        control_pattern = 'folio_wait_bit_common' if args.corpus else 'shared'
        oracle = sp.run(['rg', '-l', '-0', '--', control_pattern, '.'], cwd=root, capture_output=True, check=False)
        assert oracle.returncode in (0, 1), oracle.stderr
        original_matches = {path.removeprefix('./') for path in oracle.stdout.decode().split('\0') if path}
        added.write_text('newPublicationMarker shared\n')
        samples = {'before': [], 'after': []}
        for repetition in range(args.repetitions):
            for variant in (['before', 'after'] if repetition % 2 == 0 else ['after', 'before']):
                indexes = base / f'{segments}-{repetition}-{variant}'
                shutil.copytree(candidate_prepared if variant == 'after' else prepared, indexes)
                start = time.perf_counter()
                proc = run([binaries[variant], 'index', root], indexes, variant)
                elapsed = time.perf_counter() - start
                for pattern, expected in [('newPublicationMarker', {added.name}), (control_pattern, original_matches | ({added.name} if control_pattern == 'shared' else set()))]:
                    output = run([binaries[variant], '-l', '--color=never', f're:/{pattern}/', '-p', root], indexes, variant).stdout
                    paths = [str(Path(line).relative_to(root)) if Path(line).is_absolute() else line.removeprefix('./') for line in output.splitlines()]
                    assert len(paths) == len(set(paths)) and set(paths) == expected
                current, = indexes.rglob('CURRENT')
                generation = current.parent / 'generations' / current.read_text().strip()
                meta = json.loads((generation / 'meta.json').read_text())
                assert meta['doc_count'] == files + 1 and meta['segment_count'] == segments + 1
                samples[variant].append({'seconds': elapsed, 'stdout': proc.stdout, 'stderr': proc.stderr})
                shutil.rmtree(indexes)
        row = {'inherited_segments': segments, 'prepared_versions': {'before': original['version'], 'after': candidate_meta['version']}, 'samples': samples,
               'median_seconds': {v: statistics.median(s['seconds'] for s in values) for v, values in samples.items()}}
        result['rows'].append(row)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')
        print(segments, row['median_seconds'], flush=True)
        if args.keep_fixture:
            result['retained_fixtures'].append({'root': str(root), 'before': str(prepared), 'after': str(candidate_prepared)})
        else:
            shutil.rmtree(prepared)
            if candidate_prepared != prepared:
                shutil.rmtree(candidate_prepared)
        added.unlink()
    assert helper.corpus_manifest(root) == manifest, 'Publication corpus changed outside the probe'
    result['manifest_verified_before_and_after'] = True
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    if not args.keep_fixture:
        shutil.rmtree(root)


if __name__ == '__main__':
    main()
