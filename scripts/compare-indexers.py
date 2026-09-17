"""Stock-CLI snapshot comparison with whole-corpus coverage and exact results.

A tool with incomplete coverage is recorded but excluded from timing comparisons.
No source patches or silent result caps are accepted. Build stderr is retained.
"""
import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import random
import re
import statistics
import shutil
import subprocess as sp
import tempfile
import time


def normalize_paths(output, root):
    records = []
    for line in output.decode().splitlines():
        path = Path(line)
        records.append(str(path.relative_to(root)) if path.is_absolute() else line.removeprefix('./'))
    counts = Counter(records)
    if any(count != 1 for count in counts.values()):
        raise ValueError('Duplicate file records')
    return set(records)


def zoekt_query(pattern):
    return 'case:yes type:file content:' + json.dumps(pattern)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--corpus', type=Path, required=True)
    parser.add_argument('--fxi', type=Path, required=True)
    parser.add_argument('--go-binaries', type=Path, default=Path('/tmp/fxi-research-bin'))
    parser.add_argument('--repetitions', type=int, default=7)
    parser.add_argument('--build-repetitions', type=int, default=1)
    parser.add_argument('--suite', choices=['linux', 'redis'], default='linux')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if min(args.repetitions, args.build_repetitions) < 1:
        parser.error('repetitions must be positive')
    root = args.corpus.resolve()
    base = Path(tempfile.mkdtemp(prefix='fxi-indexer-comparison-'))
    binaries = {'fxi': args.fxi.resolve(), **{name: (args.go_binaries / name).resolve()
                for name in ['zoekt', 'zoekt-index', 'csearch', 'cindex']}}
    env = {**os.environ, 'FXI_INDEXES': str(base / 'fxi'), 'FXI_SOCKET': str(base / 'unused.sock'),
           'XDG_RUNTIME_DIR': str(base), 'CSEARCHINDEX': str(base / 'csearch.idx')}
    source_paths = sp.check_output(['rg', '--files'], cwd=root).decode().splitlines()
    manifest = hashlib.sha256()
    total_bytes = 0
    for name in sorted(source_paths):
        data = (root / name).read_bytes()
        total_bytes += len(data)
        manifest.update(json.dumps([name, hashlib.sha256(data).hexdigest()], separators=(',', ':')).encode() + b'\n')
    expected_files = set(source_paths)
    result = {'base': str(base), 'corpus': str(root), 'mode': 'direct CLI, warm filesystem, immutable snapshot',
              'fxi_source_pack': env.get('FXI_SOURCE_PACK'),
              'files': len(expected_files), 'source_bytes': total_bytes, 'manifest_sha256': manifest.hexdigest(),
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'binaries': {name: {'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
                           for name, binary in binaries.items()}, 'builds': {}, 'coverage': {}, 'rows': []}
    def save():
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + '\n')
    def run(command, codes=(0,)):
        start = time.perf_counter_ns()
        proc = sp.run([str(x) for x in command], cwd=root, env=env, capture_output=True, timeout=600)
        elapsed = (time.perf_counter_ns() - start) / 1e6
        if proc.returncode not in codes or b'Daemon search failed' in proc.stderr:
            raise RuntimeError((command, proc.returncode, proc.stderr.decode()))
        return elapsed, proc
    def command(tool, pattern):
        if tool == 'fxi':
            return [binaries[tool], '-l', '--color=never', f're:/{pattern}/', '-p', root]
        if tool == 'zoekt':
            return [binaries[tool], '-index_dir', base / 'zoekt', '-l', zoekt_query(pattern)]
        return [binaries[tool], '-l', pattern]
    def query(tool, pattern):
        elapsed, proc = run(command(tool, pattern), (0, 1) if tool == 'csearch' else (0,))
        if tool == 'csearch' and proc.stderr:
            raise RuntimeError(('csearch query diagnostics', proc.stderr.decode()))
        return elapsed, normalize_paths(proc.stdout, root)
    for repetition in range(args.build_repetitions):
        order = ['fxi', 'zoekt', 'csearch']
        random.Random(420 + repetition).shuffle(order)
        for tool in order:
            index_root = {'fxi': base / 'fxi', 'zoekt': base / 'zoekt', 'csearch': base / 'csearch.idx'}[tool]
            if index_root.is_dir():
                shutil.rmtree(index_root)
            elif index_root.exists():
                index_root.unlink()
            if tool == 'fxi':
                build = [binaries[tool], 'index', '--force', root]
            elif tool == 'zoekt':
                build = [binaries['zoekt-index'], '-index', base / 'zoekt', '-disable_ctags',
                         '-file_limit', '10000000', '-max_trigram_count', '16777216',
                         '-ignore_dirs', '.git,.hg,.svn,.tgrep', root]
            else:
                build = [binaries['cindex'], '-reset', root]
            elapsed, proc = run(['/usr/bin/time', '-l', *build])
            stderr = proc.stderr.decode()
            rss = int(re.search(r'(\d+)\s+maximum resident set size', stderr).group(1))
            result['builds'].setdefault(tool, []).append({'seconds': elapsed / 1000, 'peak_rss_bytes': rss,
                'command': [str(x) for x in build], 'stderr': stderr})
            # All fixtures contain nonempty text. Use a real content match: csearch's
            # empty-pattern special case does not enumerate files in this revision.
            _, covered = query(tool, '^')
            missing, extra = sorted(expected_files - covered), sorted(covered - expected_files)
            result['coverage'][tool] = {'indexed_files': len(covered), 'complete': not missing and not extra,
                                         'missing': missing, 'extra': extra}
            index_root = {'fxi': base / 'fxi', 'zoekt': base / 'zoekt', 'csearch': base / 'csearch.idx'}[tool]
            result['builds'][tool][-1]['index_bytes'] = (index_root.stat().st_size if index_root.is_file()
                else sum(p.stat().st_size for p in index_root.rglob('*') if p.is_file()))
            print('build', tool, round(elapsed / 1000, 3), 'seconds; missing', len(missing), 'extra', len(extra), flush=True)
            save()
    patterns = (['folio_wait_bit_common', 'auditNonexistentSymbol94283', 'struct file_operations', 'return',
                 'folio_wait_bit_common|bpf_prog_select_runtime', '.*folio_wait_bit_common'] if args.suite == 'linux'
                else ['raxFind', 'auditNonexistentSymbol94283', 'static void', 'return', 'raxFind|dictRehash', '.*raxFind'])
    eligible = [tool for tool, coverage in result['coverage'].items() if coverage['complete']]
    for pattern in patterns:
        _, oracle = run(['rg', '-l', '--color=never', pattern, '.'], (0, 1))
        expected = normalize_paths(oracle.stdout, root)
        samples = {tool: [] for tool in eligible}
        for repetition in range(-1, args.repetitions):
            order = list(eligible)
            random.Random(1729 + repetition).shuffle(order)
            for tool in order:
                elapsed, actual = query(tool, pattern)
                if actual != expected:
                    result['failure'] = {'tool': tool, 'pattern': pattern, 'missing': sorted(expected - actual), 'extra': sorted(actual - expected)}
                    save()
                    raise AssertionError(result['failure'])
                if repetition >= 0:
                    samples[tool].append(elapsed)
        row = {'pattern': pattern, 'files': len(expected), 'tools': {tool: {
            'median_ms': statistics.median(values), 'samples_ms': values} for tool, values in samples.items()}}
        result['rows'].append(row)
        print(pattern, {tool: round(data['median_ms'], 3) for tool, data in row['tools'].items()}, flush=True)
        save()


if __name__ == '__main__':
    main()
