#!/usr/bin/env python3
"""Exercise stable segment publication with an independent exhaustive source oracle.

Example (run an idle, already-built binary):
  python3 scripts/validate-stable-lifecycle.py --binary target/release/fxi \
      --steps 1000 --output /tmp/fxi-lifecycle.json

Steps are PER PROFILE; full and lean run the same seeded operation sequence.
Each block of four steps contains an edit, rename, deletion and addition in a
seeded order. The live corpus stays bounded, while revisions and segment objects
turn over. Every step compares all matching source lines for a universal marker
and the changed revision markers. Periodic audits add literal, case-insensitive,
regex, files-only and count checks. The oracle reads source bytes anew and uses
Python substring/simple regex matching, never FXI index/query code or output.

All source, application data, indexes, sockets and the executable snapshot live
in a temporary directory, removed on success or failure. Only the JSON report
persists. Timings are raw subprocess wall times, NOT a comparative benchmark;
run without concurrent compilation or other load when interpreting latency.
This is sequential CLI validation, not crash injection or long-lived-reader
concurrency testing. Checked/routing policies are disabled. Source packs are
optional, explicitly recorded, and only supported by FXI on Unix.
"""

import argparse
from collections import Counter, defaultdict
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import traceback


MARKER = 'lifecycle'
ABSENT = 'zzqAbsentLifecycleRevision987654321'


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def private_environment(base, source_pack):
    # Do not inherit experimental FXI policy, storage or socket overrides.
    env = {key: value for key, value in os.environ.items() if not key.startswith('FXI_')}
    for key, name in [('FXI_APP_DATA', 'app'), ('FXI_INDEXES', 'indexes'),
                      ('XDG_RUNTIME_DIR', 'runtime'), ('XDG_CONFIG_HOME', 'config'),
                      ('XDG_CACHE_HOME', 'cache'), ('XDG_DATA_HOME', 'data'),
                      ('TMPDIR', 'tmp'), ('TMP', 'tmp'), ('TEMP', 'tmp')]:
        directory = base / name
        directory.mkdir(parents=True, exist_ok=True)
        env[key] = str(directory)
    env.update(FXI_SOCKET=str(base / 'unused.sock'), FXI_STABLE_SEGMENTS='1',
               FXI_QUERY_LOCAL='0', FXI_NEGATIVE_ROUTING='0', FXI_GENERATION_ROUTING='0',
               FXI_SOURCE_PACK='1' if source_pack else '0', FXI_STALE_WARN_SECS='0',
               NO_COLOR='1', LC_ALL='C')
    return env


class Corpus:
    def __init__(self, root, seed, files):
        self.root = root
        self.rng = random.Random(seed)
        self.paths = {}
        self.serial = 0
        self.revision = 0
        self.block = []
        root.mkdir(parents=True)
        (root / '.git').mkdir()  # Root discovery only; no git subprocess/config.
        for _ in range(files):
            self.add()

    def new_path(self):
        self.serial += 1
        name = f'file_{self.serial:07d}'
        if self.serial % 5 == 0:
            name += ' café space'
        return Path('src') / f'group{self.serial % 7}' / (name + '.txt')

    def write_revision(self, path):
        self.revision += 1
        marker = f'revisionQ{self.revision:09d}Z'
        vocabulary = ['alpha beta', 'Alpha ALPHA', 'alphabet _alpha', 'foo/bar',
                      'foo.bar', 'café 🦀 alpha', 'neutral', 'βalpha', 'omega']
        lines = [f'{MARKER} {marker} identity']
        lines += [f'{MARKER} {marker} {self.rng.choice(vocabulary)}'
                  for _ in range(5 + self.revision % 4)]
        separator = '\r\n' if self.revision % 3 == 0 else '\n'
        content = separator.join(lines) + (separator if self.revision % 2 == 0 else '')
        destination = self.root / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(content.encode('utf-8'))
        self.paths[path] = marker
        return marker

    def add(self):
        path = self.new_path()
        return {'kind': 'add', 'new_path': path.as_posix(),
                'new_marker': self.write_revision(path)}

    def mutate(self):
        if not self.block:
            self.block = ['edit', 'rename', 'delete', 'add']
            self.rng.shuffle(self.block)
        kind = self.block.pop()
        if kind == 'add':
            return self.add()
        path = self.rng.choice(sorted(self.paths))
        marker = self.paths[path]
        operation = {'kind': kind, 'old_path': path.as_posix(), 'old_marker': marker}
        if kind == 'edit':
            operation.update(new_path=path.as_posix(), new_marker=self.write_revision(path))
        elif kind == 'rename':
            new_path = self.new_path()
            (self.root / new_path).parent.mkdir(parents=True, exist_ok=True)
            (self.root / path).rename(self.root / new_path)
            del self.paths[path]
            self.paths[new_path] = marker
            operation.update(new_path=new_path.as_posix(), new_marker=marker)
        else:
            (self.root / path).unlink()
            del self.paths[path]
        return operation


def read_sources(root):
    return {path.relative_to(root).as_posix(): path.read_bytes().decode('utf-8').splitlines()
            for path in sorted(root.rglob('*.txt'))}


def source_matches(sources, pattern, mode='literal', insensitive=False):
    expression = None
    if mode == 'regex' or insensitive:
        expression = re.compile(pattern if mode == 'regex' else re.escape(pattern),
                                re.IGNORECASE if insensitive else 0)
    rows = []
    for path, lines in sorted(sources.items()):
        for line_number, line in enumerate(lines, 1):
            if expression:
                match = expression.search(line)
                span = match.span() if match else None
            else:
                start = line.find(pattern)
                span = (start, start + len(pattern)) if start >= 0 else None
            if span is not None:
                start, end = span
                rows.append({'path': path, 'line_number': line_number, 'line_content': line,
                             'match_start': len(line[:start].encode('utf-8')),
                             'match_end': len(line[:end].encode('utf-8')),
                             'context_before': [], 'context_after': []})
    return rows


def normalize_path(value, root):
    path = Path(value)
    if path.is_absolute():
        path = path.relative_to(root)
    require('..' not in path.parts, f'Output path escapes source root: {value!r}')
    return path.as_posix()


def verify_response(response, expected, root, output_mode):
    counts = Counter(row['path'] for row in expected)
    require(response.get('files_with_matches') == len(counts), 'Wrong files_with_matches')
    if output_mode == 'files':
        actual = [normalize_path(path, root) for path in response['file_paths']]
        require(sorted(actual) == sorted(counts), 'Files-only membership/duplicates differ')
    elif output_mode == 'counts':
        actual = [(normalize_path(path, root), count) for path, count in response['file_counts']]
        require(sorted(actual) == sorted(counts.items()), 'Per-file counts differ')
    else:
        actual = []
        for row in response['matches']:
            normalized = {key: row[key] for key in expected_row_keys()}
            normalized['path'] = normalize_path(row['path'], root)
            actual.append(normalized)
        key = lambda row: (row['path'], row['line_number'])
        require(sorted(actual, key=key) == sorted(expected, key=key),
                f'Content differs: expected {len(expected)} rows, got {len(actual)}')


def expected_row_keys():
    return ('path', 'line_number', 'line_content', 'match_start', 'match_end',
            'context_before', 'context_after')


def tree_bytes(path):
    logical = allocated = 0
    for file in path.rglob('*'):
        require(not file.is_symlink(), f'Unexpected storage symlink: {file}')
        if file.is_file():
            stat = file.stat()
            logical += stat.st_size
            allocated += getattr(stat, 'st_blocks', 0) * 512
    return logical, allocated


def storage_snapshot(indexes, profile, details=False):
    currents = list(indexes.glob('*/CURRENT'))
    require(len(currents) == 1, f'Expected exactly one private CURRENT, found {currents}')
    current = currents[0]
    container = current.parent
    current_name = current.read_text().strip()
    objects = container / 'objects'
    require(objects.is_dir() and not objects.is_symlink(), 'Missing stable object store')
    all_objects = set()
    for entry in objects.iterdir():
        require(entry.is_dir() and not entry.is_symlink(), f'Unexpected object entry: {entry}')
        all_objects.add(entry.name)
    referenced = set()
    current_refs = None
    meta = None
    generations = list((container / 'generations').iterdir())
    for generation in generations:
        require(generation.is_dir() and not generation.is_symlink(), 'Invalid generation entry')
        require((generation / 'objects.check').is_file(), 'Missing stable manifest binding')
        manifest = json.loads((generation / 'meta.json').read_text())
        require(manifest['version'] == (4 if profile == 'full' else 5), 'Wrong stable format')
        ids = ([manifest['base_segment']] if manifest['base_segment'] is not None else [])
        ids += manifest['delta_segments']
        mapping = manifest.get('segment_objects', {})
        require(len(ids) == len(set(ids)) == manifest['segment_count'] == len(mapping),
                'Invalid segment count/identity')
        require(set(map(str, ids)) == set(mapping), 'Incomplete object manifest')
        refs = set(mapping.values())
        require(len(refs) == len(mapping) and refs <= all_objects, 'Aliased/dangling object references')
        require(not any((generation / 'segments').glob('*')), 'Stable generation retains local segments')
        referenced.update(refs)
        if generation.name == current_name:
            meta, current_refs = manifest, refs
    require(meta is not None, 'CURRENT generation missing')
    logical, allocated = tree_bytes(container)
    object_bytes = sum(tree_bytes(objects / name)[0] for name in all_objects)
    orphaned = all_objects - referenced
    snapshot = {'current_generation': current_name,
                'generation_count': len(generations), 'object_count': len(all_objects),
                'current_object_count': len(current_refs), 'orphan_count': len(orphaned),
                'noncurrent_object_count': len(all_objects - current_refs),
                'logical_bytes': logical, 'allocated_bytes': allocated,
                'object_logical_bytes': object_bytes, 'segment_count': meta['segment_count'],
                'doc_count': meta['doc_count'], 'valid_doc_count': meta['valid_doc_count'],
                'tombstone_count': meta['tombstone_count']}
    if details:
        snapshot.update(all_objects=sorted(all_objects),
                        current_objects=sorted(current_refs), orphan_objects=sorted(orphaned))
    return snapshot


class Runner:
    def __init__(self, binary, root, env, report, profile, timeout):
        self.binary, self.root, self.env = binary, root, env
        self.report, self.profile, self.timeout = report, profile, timeout
        self.step = 0

    def run(self, args, role):
        started = time.perf_counter_ns()
        sample = {'profile': self.profile, 'step': self.step, 'role': role, 'args': args}
        self.report['samples'].append(sample)
        try:
            result = subprocess.run([str(self.binary), *args], cwd=self.root, env=self.env,
                                    capture_output=True, text=True, encoding='utf-8',
                                    timeout=self.timeout, check=False)
        except BaseException as error:
            sample.update(elapsed_ns=time.perf_counter_ns() - started, error=repr(error))
            raise
        sample.update(elapsed_ns=time.perf_counter_ns() - started, returncode=result.returncode)
        if result.returncode:
            sample.update(stdout=result.stdout, stderr=result.stderr)
            raise RuntimeError(f'{role} failed ({result.returncode}): {args}\n{result.stderr}')
        if role != 'search':
            sample.update(stdout=result.stdout, stderr=result.stderr)
        return result.stdout

    def check(self, sources, pattern, mode='literal', insensitive=False, output_mode='content'):
        args = ['--json', '--color=never', '-m', '0', '-F' if mode == 'literal' else '--regex']
        if insensitive:
            args.append('-i')
        if output_mode != 'content':
            args.append('-l' if output_mode == 'files' else '-c')
        args += ['-e', pattern, '.']
        output = self.run(args, 'search')
        expected = source_matches(sources, pattern, mode, insensitive)
        try:
            verify_response(json.loads(output), expected, self.root, output_mode)
        except (AssertionError, KeyError, TypeError, ValueError):
            self.report['failure_query'] = {'profile': self.profile, 'step': self.step,
                                           'args': args, 'expected': expected, 'actual': output}
            raise
        self.report['checks']['queries'] += 1
        self.report['checks']['matched_rows'] += len(expected)
        self.report['checks'][output_mode] += 1

    def audit(self, corpus, operation=None, wide=False):
        sources = read_sources(self.root)
        require(set(sources) == {path.as_posix() for path in corpus.paths}, 'Unexpected source membership')
        self.report['checks']['source_snapshots'] += 1
        self.report['checks']['source_files_read'] += len(sources)
        patterns = [MARKER]
        if operation:
            patterns.extend(operation[key] for key in ('old_marker', 'new_marker') if key in operation)
        for pattern in dict.fromkeys(patterns):
            self.check(sources, pattern)
        if wide:
            for pattern, mode, insensitive in [('alpha', 'literal', False), ('ALPHA', 'literal', True),
                                                ('foo[/.]bar|beta', 'regex', False),
                                                ('^lifecycle.*omega$', 'regex', False),
                                                ('café', 'literal', False), (ABSENT, 'literal', False)]:
                self.check(sources, pattern, mode, insensitive)
            self.check(sources, MARKER, output_mode='files')
            self.check(sources, 'alpha', output_mode='counts')


def latency_summary(samples):
    grouped = defaultdict(list)
    for sample in samples:
        grouped[(sample['profile'], sample['role'])].append(sample['elapsed_ns'] / 1e6)
    result = {}
    for (profile, role), values in sorted(grouped.items()):
        ordered = sorted(values)
        result[f'{profile}/{role}'] = {'count': len(values), 'median_ms': statistics.median(values),
                                     'p95_ms': ordered[math.ceil(len(values) * .95) - 1],
                                     'max_ms': max(values), 'total_ms': sum(values)}
    return result


def save_report(path, report):
    report['latency'] = latency_summary(report['samples'])
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.tmp')
    temporary.write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n', encoding='utf-8')
    temporary.replace(path)


def run_profile(base, binary, args, report, profile):
    workspace = base / profile
    env = private_environment(workspace, args.source_pack)
    corpus = Corpus(workspace / 'repo', args.seed, args.files)
    runner = Runner(binary, corpus.root, env, report, profile, args.timeout)
    result = {'profile': profile, 'operations': [], 'compactions': [], 'operation_counts': {}}
    report['profiles'].append(result)
    runner.run(['index', '--force', '--profile', profile, '--chunk-size', str(args.chunk_size)], 'build')
    runner.audit(corpus, wide=True)
    indexes = Path(env['FXI_INDEXES'])
    result['initial_storage'] = storage_snapshot(indexes, profile, details=True)
    report['checks']['storage_snapshots'] += 1
    previous_generation = result['initial_storage']['current_generation']
    for step in range(1, args.steps + 1):
        runner.step = step
        operation = corpus.mutate()
        runner.run(['index'], 'update')
        checks_before = report['checks']['queries']
        runner.audit(corpus, operation, wide=step % args.audit_every == 0)
        snapshot = storage_snapshot(indexes, profile)
        # Retain the failing operation/storage evidence before checking it: the
        # private fixture is deliberately cleaned even when an invariant fails.
        result['operations'].append({'step': step, **operation, 'storage': snapshot,
                                     'queries_checked': report['checks']['queries'] - checks_before})
        require(snapshot['current_generation'] != previous_generation, 'Changed source was not published')
        previous_generation = snapshot['current_generation']
        require(snapshot['valid_doc_count'] == len(corpus.paths), 'Live document count differs from source')
        require(snapshot['orphan_count'] == 0, 'Successful publication left unreachable objects')
        report['checks']['storage_snapshots'] += 1
        if step % args.compact_every == 0 and step != args.steps:
            runner.run(['compact'], 'compact')
            runner.audit(corpus, wide=True)
            compacted = storage_snapshot(indexes, profile)
            previous_generation = compacted['current_generation']
            require(compacted['segment_count'] == 1 and compacted['tombstone_count'] == 0
                    and compacted['doc_count'] == len(corpus.paths), 'Compaction did not remove fragmentation')
            result['compactions'].append({'step': step, 'storage': compacted})
            report['checks']['compactions'] += 1
            report['checks']['storage_snapshots'] += 1
        if step % args.checkpoint_every == 0:
            result['completed_steps'] = step
            save_report(args.output, report)
            print(f'{profile}: {step}/{args.steps} steps, {report["checks"]["queries"]} queries checked', flush=True)
    before_final = storage_snapshot(indexes, profile, details=True)
    runner.run(['compact'], 'final_compact')
    runner.audit(corpus, wide=True)
    compacted = storage_snapshot(indexes, profile, details=True)
    require(compacted['segment_count'] == 1 and compacted['tombstone_count'] == 0
            and compacted['doc_count'] == len(corpus.paths), 'Final compaction failed')
    # An additional real publication gives cleanup another opportunity after
    # the compactor's source lease has closed; a no-change `index` would not.
    runner.step = args.steps + 1
    operation = corpus.add()
    runner.run(['index'], 'reclamation_update')
    runner.audit(corpus, operation, wide=True)
    final = storage_snapshot(indexes, profile, details=True)
    require(final['valid_doc_count'] == len(corpus.paths), 'Final live document count differs')
    require(final['all_objects'] == final['current_objects'], 'Retired segment objects survived final publication')
    retired = set(before_final['all_objects']) - set(compacted['current_objects'])
    require(not retired.intersection(final['all_objects']), 'Pre-compaction objects were not reclaimed')
    report['checks']['compactions'] += 1
    report['checks']['reclamations'] += 1
    report['checks']['storage_snapshots'] += 3
    result.update(completed_steps=args.steps, operation_counts=dict(Counter(row['kind'] for row in result['operations'])),
                  before_final_compaction=before_final, after_final_compaction=compacted,
                  final_storage=final, final_source_files=len(corpus.paths), reclamation_operation=operation,
                  reclaimed_object_count=len(retired),
                  peak_logical_bytes=max(row['storage']['logical_bytes'] for row in result['operations']),
                  peak_object_count=max(row['storage']['object_count'] for row in result['operations']))
    workload = [{key: value for key, value in row.items() if key not in ('storage', 'queries_checked')}
                for row in result['operations']]
    result['workload_sha256'] = hashlib.sha256(json.dumps(workload, sort_keys=True).encode()).hexdigest()
    source_manifest = [(path.relative_to(corpus.root).as_posix(), sha256(path))
                       for path in sorted(corpus.root.rglob('*.txt'))]
    result['final_source_manifest_sha256'] = hashlib.sha256(json.dumps(source_manifest).encode()).hexdigest()
    for previous in report['profiles'][:-1]:
        require(previous['workload_sha256'] == result['workload_sha256']
                and previous['final_source_manifest_sha256'] == result['final_source_manifest_sha256'],
                'Profiles did not receive identical seeded workloads')
    save_report(args.output, report)


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--steps', type=int, default=1000, help='Lifecycle steps per profile')
    parser.add_argument('--seed', type=int, default=0x5EED94283)
    parser.add_argument('--profiles', nargs='+', choices=['full', 'lean'], default=['full', 'lean'])
    parser.add_argument('--files', type=int, default=64, help='Initial files; at least 16 avoids rebuild thresholds')
    parser.add_argument('--chunk-size', type=int, default=16)
    parser.add_argument('--compact-every', type=int, default=50)
    parser.add_argument('--audit-every', type=int, default=25)
    parser.add_argument('--checkpoint-every', type=int, default=100)
    parser.add_argument('--timeout', type=float, default=120, help='Seconds per CLI subprocess')
    parser.add_argument('--source-pack', action='store_true')
    args = parser.parse_args(argv)
    if args.files < 16 or any(getattr(args, field) < 1 for field in
                             ['steps', 'chunk_size', 'compact_every', 'audit_every', 'checkpoint_every']) or args.timeout <= 0:
        parser.error('files must be >=16; steps, intervals, chunk-size and timeout must be positive')
    if len(set(args.profiles)) != len(args.profiles):
        parser.error('profiles must be distinct')
    if args.source_pack and os.name != 'posix':
        parser.error('--source-pack requires Unix')
    args.binary, args.output = args.binary.resolve(), args.output.resolve()
    if not args.binary.is_file():
        parser.error('binary must be an existing executable file')
    if args.binary in (args.output, args.output.with_name(args.output.name + '.tmp')):
        parser.error('report and report temporary path must not overwrite the binary')
    return args


def main(argv=None):
    args = parse_args(argv)
    report = {'schema': 1, 'status': 'running', 'started_utc': datetime.now(timezone.utc).isoformat(),
              'seed': args.seed, 'steps_per_profile': args.steps, 'initial_files': args.files,
              'compact_every': args.compact_every, 'audit_every': args.audit_every,
              'chunk_size': args.chunk_size, 'source_pack': args.source_pack,
              'policy': 'stable segments, strict validation, no routing certificates',
              'timing_note': 'Diagnostic subprocess wall times only. Do not infer comparative benchmark results; run without concurrent compilation/load.',
              'platform': platform.platform(), 'python': sys.version, 'cpu_count': os.cpu_count(),
              'load_start': list(os.getloadavg()) if hasattr(os, 'getloadavg') else None,
              'effective_thread_environment': {key: value for key, value in os.environ.items()
                                               if key == 'RAYON_NUM_THREADS'},
              'binary': {'path': str(args.binary)}, 'harness_sha256': sha256(Path(__file__)),
              'checks': dict.fromkeys(['queries', 'matched_rows', 'content', 'files', 'counts',
                                      'source_snapshots', 'source_files_read', 'storage_snapshots',
                                      'compactions', 'reclamations'], 0), 'samples': [], 'profiles': [],
              'limitations': ['Sequential CLI processes; no long-lived reader leases or concurrent writers.',
                              'No power-loss/fault injection; no migration from legacy format.',
                              'Synthetic bounded text corpus; simple shared Python/Rust regex subset.',
                              'Storage bytes are logical; allocated bytes use st_blocks when available.']}
    base = None
    started = time.perf_counter()
    code = 0
    try:
        # Keep Unix socket paths below common sockaddr_un limits even on macOS.
        with tempfile.TemporaryDirectory(prefix='fxi-life-', dir='/tmp' if os.name == 'posix' else None) as temporary:
            base = Path(temporary).resolve()
            report['temporary_workspace'] = str(base)
            binary = base / args.binary.name
            before = sha256(args.binary)
            shutil.copy2(args.binary, binary)
            snapshot_hash = sha256(binary)
            require(before == snapshot_hash == sha256(args.binary), 'Binary changed while taking executable snapshot')
            report['binary']['sha256'] = snapshot_hash
            for profile in args.profiles:
                run_profile(base, binary, args, report, profile)
            report['status'] = 'passed'
    except BaseException as error:
        code = 130 if isinstance(error, KeyboardInterrupt) else 1
        report.update(status='failed', failure=repr(error), traceback=traceback.format_exc())
        print(f'Lifecycle validation failed: {error}', file=sys.stderr)
    finally:
        report.update(workspace_cleaned=base is None or not base.exists(),
                      elapsed_seconds=time.perf_counter() - started,
                      finished_utc=datetime.now(timezone.utc).isoformat(),
                      load_end=list(os.getloadavg()) if hasattr(os, 'getloadavg') else None)
        save_report(args.output, report)
    print(f'{report["status"]}: {report["checks"]["queries"]} oracle queries; report {args.output}', flush=True)
    return code


if __name__ == '__main__':
    sys.exit(main())
