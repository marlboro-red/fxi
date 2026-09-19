"""Compare stock search CLIs on an unmodified Chromium checkout.

Keep different coverage visible: timings with incorrect result sets are retained
as diagnostic samples, never counted as performance wins. No cache flushing or
dependency checkout is performed. Index storage and daemons are private.
"""
import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import shutil
import statistics
import subprocess as sp
import time


WORKLOADS = [
    ('rare', 'RenderFrameHostImpl::DidCommitNavigation', 'literal', False),
    ('absent', 'fxiChromiumAbsentSymbol8f692a4d', 'literal', False),
    ('phrase', 'class Browser', 'literal', False),
    ('namespace', 'namespace content', 'literal', False),
    ('common', 'std::unique_ptr', 'literal', False),
    ('very-common', 'return', 'literal', False),
    ('punctuation', 'DCHECK(', 'literal', False),
    ('case-insensitive', 'todo', 'literal', True),
    ('short', 'zx', 'literal', False),
    ('alternation', 'RenderFrameHostImpl|BrowserMainLoop', 'regex', False),
    ('regex', 'class [A-Za-z_]+Browser', 'regex', False),
    ('regex-no-prefix', '.*RenderFrameHostImpl', 'regex', False),
]


def paths(data, root):
    names = []
    for name in data.decode('utf-8').splitlines():
        path = Path(name)
        names.append(str(path.relative_to(root)) if path.is_absolute() else name.removeprefix('./'))
    if any(count != 1 for count in Counter(names).values()):
        raise ValueError('Duplicate matching filenames')
    return set(names)


def difference(actual, expected):
    missing, extra = sorted(expected - actual), sorted(actual - expected)
    return {'exact': not missing and not extra, 'files': len(actual),
            'missing': missing, 'extra': extra}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--corpus', required=True, type=Path)
    p.add_argument('--fxi', required=True, type=Path)
    p.add_argument('--fxi-revision', required=True)
    p.add_argument('--tgrep', type=Path, default=Path('/tmp/fxi-audit-tgrep/target/release/tgrep'))
    p.add_argument('--go-binaries', type=Path, default=Path('/tmp/fxi-research-bin'))
    p.add_argument('--work', required=True, type=Path)
    p.add_argument('--output', required=True, type=Path)
    p.add_argument('--repetitions', type=int, default=7)
    p.add_argument('--reuse', action='store_true', help='Resume this exact benchmark and existing indexes')
    a = p.parse_args()
    if a.repetitions < 1:
        p.error('repetitions must be positive')
    root, base = a.corpus.resolve(), a.work.resolve()
    base.mkdir(parents=True, exist_ok=True)
    a.output.parent.mkdir(parents=True, exist_ok=True)
    bins = {'fxi': a.fxi.resolve(), 'tgrep': a.tgrep.resolve(),
            'ripgrep': Path(shutil.which('rg')),
            **{name: (a.go_binaries / name).resolve()
               for name in ['zoekt', 'zoekt-index', 'csearch', 'cindex']}}
    env = {k: v for k, v in os.environ.items()
           if not k.startswith('FXI_') and k not in
           ('RIPGREP_CONFIG_PATH', 'RAYON_NUM_THREADS', 'CSEARCHINDEX')}
    env.update(FXI_APP_DATA=str(base / 'app'), FXI_INDEXES=str(base / 'default'),
               FXI_SOCKET=str(base / 'unused.sock'), XDG_RUNTIME_DIR=str(base),
               CSEARCHINDEX=str(base / 'csearch.idx'), NO_COLOR='1')
    flags = ['FXI_QUERY_LOCAL', 'FXI_GENERATION_ROUTING', 'FXI_SOURCE_PACK',
             'FXI_SOURCE_PACK_COMPRESSION', 'FXI_STABLE_SEGMENTS']

    def environment(tool):
        child = dict(env)
        if tool.startswith('fxi'):
            opt = 'opt' in tool
            child.update({flag: '1' if opt else '0' for flag in flags})
            child['FXI_INDEXES'] = str(base / ('opt' if opt else 'default'))
            if tool.endswith('resident'):
                child['FXI_SOCKET'] = str(base / ('opt.sock' if opt else 'default.sock'))
        return child

    def run(command, child_env=env, timeout=1800):
        started = time.perf_counter_ns()
        proc = sp.run([str(x) for x in command], cwd=root, env=child_env,
                      capture_output=True, timeout=timeout)
        return (time.perf_counter_ns() - started) / 1e6, proc

    def query(tool, pattern, mode='regex', insensitive=False):
        regex = re.escape(pattern) if mode == 'literal' else pattern
        if tool.startswith('fxi'):
            cmd = [bins['fxi'], '-l', '--color=never',
                   '-F' if mode == 'literal' else '--regex']
            if insensitive:
                cmd.append('-i')
            cmd += ['-e', pattern, '-p', root]
        elif tool == 'zoekt':
            q = ('case:no' if insensitive else 'case:yes') + ' type:file content:' + json.dumps(regex)
            cmd = [bins[tool], '-index_dir', base / 'zoekt', '-l', q]
        elif tool == 'csearch':
            cmd = [bins[tool], '-l'] + (['-i'] if insensitive else []) + [regex]
        else:
            cmd = [bins[tool], '-l', '--color=never']
            if tool == 'tgrep':
                cmd.append('-i' if insensitive else '-s')
            elif insensitive:
                cmd.append('-i')
            if mode == 'literal':
                cmd.append('-F')
            cmd += ['--', pattern, str(root) if tool == 'tgrep' else '.']
        ms, proc = run(cmd, environment(tool))
        allowed = (0,) if tool.startswith('fxi') or tool == 'zoekt' else (0, 1)
        if proc.returncode not in allowed or b'Daemon search failed' in proc.stderr:
            raise RuntimeError((tool, cmd, proc.returncode, proc.stderr.decode(errors='replace')))
        return ms, paths(proc.stdout, root), proc.stderr.decode(errors='replace')

    revision = sp.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
    shallow = sp.check_output(['git', 'rev-parse', '--is-shallow-repository'], cwd=root, text=True).strip()
    tracked = sp.check_output(['git', 'ls-files', '-z'], cwd=root).split(b'\0')[:-1]
    if any(b'\n' in name or b'\r' in name for name in tracked):
        raise ValueError('Line-oriented competitor output cannot represent these paths')
    status = sp.check_output(['git', 'status', '--porcelain', '--untracked-files=no'], cwd=root, text=True)
    if status:
        raise ValueError('Checkout differs from Git tree (including possible case collisions): ' + status[:2000])
    result = {'corpus': str(root), 'revision': revision, 'shallow': shallow,
              'tracked_paths': len(tracked), 'dependencies_fetched': False,
              'fxi_revision': a.fxi_revision, 'platform': platform.platform(),
              'cpu': sp.check_output(['sysctl', '-n', 'machdep.cpu.brand_string'], text=True).strip(),
              'memory_bytes': int(sp.check_output(['sysctl', '-n', 'hw.memsize'], text=True)),
              'mode': 'complete files-only CLI output; warm filesystem; no cache flushing; fresh CLI processes; resident variants explicitly labeled',
              'opt_in': {flag: '1' for flag in flags}, 'opt_profile': 'lean',
              'repetitions': a.repetitions, 'build_repetitions': 1,
              'base': str(base), 'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'binaries': {k: {'path': str(v), 'sha256': hashlib.sha256(v.read_bytes()).hexdigest()}
                           for k, v in bins.items()}, 'builds': {}, 'coverage': {}, 'rows': [], 'updates': {}}
    if a.reuse and a.output.exists():
        previous = json.loads(a.output.read_text())
        for key in ['revision', 'fxi_revision', 'binaries', 'base', 'opt_in']:
            if previous[key] != result[key]:
                raise ValueError('Resume identity changed: ' + key)
        result = previous

    def save():
        temporary = a.output.with_suffix('.tmp')
        temporary.write_text(json.dumps(result, indent=2) + '\n')
        temporary.replace(a.output)

    save()
    builds = {
        'fxi-default': ([bins['fxi'], 'index', '--force', root], base / 'default'),
        'fxi-opt': ([bins['fxi'], 'index', '--force', '--profile', 'lean', root], base / 'opt'),
        'tgrep': ([bins['tgrep'], 'index', '--force', root], root / '.tgrep'),
        'zoekt': ([bins['zoekt-index'], '-index', base / 'zoekt', '-disable_ctags',
                   '-file_limit', '10485760', '-max_trigram_count', '16777216',
                   '-ignore_dirs', '.git,.hg,.svn,.tgrep', root], base / 'zoekt'),
        'csearch': ([bins['cindex'], '-reset', root], base / 'csearch.idx'),
    }
    for tool, (cmd, index) in builds.items():
        if tool in result['builds']:
            continue
        free = shutil.disk_usage(base).free
        if free < 4 * 1024**3:
            result['builds'][tool] = {'skipped': 'Less than 4 GiB free disk', 'free_bytes': free}
            save()
            continue
        print('Building', tool, flush=True)
        ms, proc = run(['/usr/bin/time', '-l', *cmd], environment(tool))
        stderr = proc.stderr.decode(errors='replace')
        rss = re.search(r'(\d+)\s+maximum resident set size', stderr)
        size = index.stat().st_size if index.is_file() else sum(
            file.stat().st_size for file in index.rglob('*') if file.is_file())
        result['builds'][tool] = {'command': [str(x) for x in cmd], 'seconds': ms / 1000,
            'exit': proc.returncode, 'peak_rss_bytes': int(rss[1]) if rss else None,
            'index_bytes': size, 'stdout': proc.stdout.decode(errors='replace'), 'stderr': stderr}
        save()
        print(tool, 'seconds', round(ms / 1000, 3), 'index MiB', round(size / 1024**2, 1),
              'exit', proc.returncode, flush=True)
    tools = ['ripgrep'] + [tool for tool, row in result['builds'].items() if row.get('exit') == 0]
    servers = []
    try:
        for tool in ('fxi-default', 'fxi-opt'):
            if tool not in tools:
                continue
            resident = tool + '-resident'
            child = environment(resident)
            address = Path(child['FXI_SOCKET'])
            address.unlink(missing_ok=True)
            log = base / (resident + '.log')
            with log.open('w') as output:
                server = sp.Popen([str(bins['fxi']), 'daemon', 'foreground'],
                                  cwd=root, env=child, stdout=output, stderr=output)
            servers.append(server)
            deadline = time.monotonic() + 30
            while not address.exists():
                if server.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError(log.read_text())
                time.sleep(.02)
            tools.append(resident)
        _, covered, _ = query('ripgrep', '^')
        result['oracle_nonempty_matching_files'] = len(covered)
        for tool in tools:
            elapsed, actual, stderr = query(tool, '^')
            result['coverage'][tool] = dict(difference(actual, covered), first_probe_ms=elapsed,
                                             stderr=stderr)
            save()
            print('Coverage', tool, len(actual), 'missing', len(covered - actual),
                  'extra', len(actual - covered), flush=True)
        completed = {row['name'] for row in result['rows']}
        for name, pattern, mode, insensitive in WORKLOADS:
            if name in completed:
                continue
            _, expected, _ = query('ripgrep', pattern, mode, insensitive)
            samples = {tool: [] for tool in tools}
            checks = {}
            diagnostics = {}
            for repetition in range(-1, a.repetitions):
                order = list(tools)
                random.Random(7291 + repetition).shuffle(order)
                for tool in order:
                    ms, actual, stderr = query(tool, pattern, mode, insensitive)
                    check = difference(actual, expected)
                    if tool in checks and checks[tool] != check:
                        raise AssertionError(('Unstable results', tool, pattern))
                    checks[tool] = check
                    if stderr:
                        diagnostics[tool] = stderr
                    if repetition >= 0:
                        samples[tool].append(ms)
            row = {'name': name, 'pattern': pattern, 'search_mode': mode,
                   'case_insensitive': insensitive, 'oracle_files': len(expected),
                   'tools': {tool: {'samples_ms': values, 'median_ms': statistics.median(values),
                                    **checks[tool]} for tool, values in samples.items()},
                   'diagnostics': diagnostics}
            result['rows'].append(row)
            save()
            print(name, {tool: (round(value['median_ms'], 2), value['exact'])
                         for tool, value in row['tools'].items()}, flush=True)
    finally:
        for server in servers:
            server.terminate()
        for server in servers:
            try:
                server.wait(timeout=20)
            except sp.TimeoutExpired:
                server.kill()
                server.wait()
    for tool in ('fxi-default', 'fxi-opt'):
        if result['builds'][tool].get('exit') != 0:
            continue
        samples = []
        for _ in range(3):
            ms, proc = run(['/usr/bin/time', '-l', bins['fxi'], 'index', root], environment(tool))
            if proc.returncode:
                raise RuntimeError(proc.stderr.decode(errors='replace'))
            samples.append({'ms': ms, 'stdout': proc.stdout.decode(errors='replace'),
                            'stderr': proc.stderr.decode(errors='replace')})
        result['updates'][tool] = {'type': 'no-change reconciliation; 3 samples', 'samples': samples,
                                   'median_ms': statistics.median(row['ms'] for row in samples)}
        save()
    result['final_tracked_status'] = sp.check_output(
        ['git', 'status', '--porcelain', '--untracked-files=no'], cwd=root, text=True)
    if result['final_tracked_status']:
        raise AssertionError('Tracked source changed during benchmark')
    result['completed'] = True
    save()


if __name__ == '__main__':
    main()
