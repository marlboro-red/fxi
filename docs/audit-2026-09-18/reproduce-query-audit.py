#!/usr/bin/env python3
"""Isolated query audit; optionally aborts ONLY its own disposable daemon.

Run: python3 docs/audit-2026-09-18/reproduce-query-audit.py --daemon-probes
No builds, benchmarks, existing indexes, or existing daemons are touched.
"""
import argparse
import datetime
import hashlib
import itertools
import json
import os
from pathlib import Path
import random
import resource
import socket
import struct
import subprocess
import tempfile
import time

p = argparse.ArgumentParser()
p.add_argument('--binary', default='target/release/fxi')
p.add_argument('--daemon-probes', action='store_true')
args = p.parse_args()
binary = str(Path(args.binary).resolve())
base = Path(tempfile.mkdtemp(prefix='fxi-query-audit-'))
root = base / 'corpus'
root.mkdir()
env = os.environ.copy()
env.update(FXI_INDEXES=str(base / 'indexes'), FXI_SOCKET='/tmp/' + base.name + '.sock')
report = {'binary': binary, 'sha256': hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
          'base': str(base), 'cli': [], 'wire': []}
files = {'a.rs': 'foo bar\nneedle\n', 'b.py': 'foo\n', 'c.rs': 'bar\n',
         'near.txt': 'beta\nx\nalpha\nx\ngamma\n', 'slash.txt': 'foo/bar\n',
         'punct.txt': 'foo-bar\nfoo.bar\nfoo()\n',
         'negative.txt': 'irrelevant\nfoo\n', 'eight.txt': '1234567\n'}
for name, content in files.items():
    (root / name).write_text(content)
for name, date in [('day_before.txt', '2101-03-01'), ('day_after.txt', '2101-03-02'),
                   ('midnight.txt', '2026-09-19'), ('invalid_date.txt', '2026-03-03')]:
    path = root / name
    path.write_text('datecheck\n')
    stamp = datetime.datetime.fromisoformat(date).replace(tzinfo=datetime.timezone.utc).timestamp()
    os.utime(path, (stamp, stamp))
subprocess.run([binary, 'index', str(root)], env=env, capture_output=True, check=True)
queries = ['foo bar', 'foo | bar', 'foo -absent', 'foo -absent line:1',
           'near:alpha,beta,gamma,2', r're:/foo\/bar/', 'foo) bar',
           'ext:rs | ext:py', '-ext:rs', 'ext:rs foo | ext:py bar', 'foo-bar', 'foo.bar',
           'size:>8', 'size:<8', 'mtime:2101-03-01', 'mtime:2026-09-18',
           'mtime:2026-02-31', 'mtime:18446744073709551615']
for query, mode in itertools.product(queries, [[], ['-c'], ['-l']]):
    cmd = [binary, '-p', str(root), *mode, '--color', 'never', query]
    result = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=10)
    report['cli'].append({'query': query, 'mode': mode, 'returncode': result.returncode,
                          'stdout': result.stdout, 'stderr': result.stderr})

# Independent rg oracle for conservative regex planning and Unicode folding.
grid = base / 'grid'
grid.mkdir()
rng = random.Random(47)
words = ['foo', 'bar', 'FOO', 'foobar', 'food', 'baz', 'école', 'ÉCOLE', 'Kelvin',
         'kelvin', 'ſample', 'sample', 'Σigma', 'σigma', 'ςigma', 'İtest', 'itest',
         'http://a/b', 'abc.def', 'alpha-beta']
for i in range(80):
    (grid / f'f{i:03}.txt').write_text('\n'.join(' '.join(rng.choices(words, k=3))
                                               for _ in range(3)) + '\n')
subprocess.run([binary, 'index', str(grid)], env=env, capture_output=True, check=True)
patterns = words[:17] + ['foo.*bar', 'foo|bar', 'foo(?:bar)?', '(?:foo|école).*bar',
    '[a-z]{3}', '^foo', 'bar$', r'\bfoo\b', r'foo\s+bar', '(?i)kelvin', '(?i)sample',
    '(?i)σigma', '(?i)école', '(?i)itest', '[Kk]elvin', 'a*', 'foo(?:x|bar)*',
    '(?:foo){1,3}', '(?:a|foobar)', '(?s)foo.bar', r'[^\n]*foo', r'\p{Greek}+',
    'f[oO]{2}', '.{0,2}foo']
report['regex_grid'] = {'comparisons': 0, 'mismatches': []}
for pattern, insensitive in itertools.product(patterns, [False, True]):
    flags = ['-i'] if insensitive else []
    actual = subprocess.run([binary, '-p', str(grid), '-l', *flags, 're:/' + pattern + '/'],
                            env=env, capture_output=True, text=True, timeout=10)
    expected = subprocess.run(['rg', '-l', *flags, pattern, '.'], cwd=grid,
                              capture_output=True, text=True, timeout=10)
    a = set(actual.stdout.splitlines())
    e = {s.removeprefix('./') for s in expected.stdout.splitlines()}
    report['regex_grid']['comparisons'] += 1
    if a != e or actual.returncode not in (0, 1):
        report['regex_grid']['mismatches'].append({'pattern': pattern, 'insensitive': insensitive,
            'fxi_only': sorted(a - e), 'rg_only': sorted(e - a), 'stderr': actual.stderr})

if args.daemon_probes:
    wire_root = base / 'wire'
    wire_root.mkdir()
    (wire_root / 'a.txt').write_text('foo\n' * 150)
    subprocess.run([binary, 'index', str(wire_root)], env=env, capture_output=True, check=True)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    log_path = base / 'daemon.log'
    with log_path.open('w') as log:
        daemon = subprocess.Popen([binary, 'daemon', 'foreground'], env=env, stdout=log, stderr=log)
        def receive_exact(stream, size):
            chunks = bytearray()
            while len(chunks) < size:
                part = stream.recv(size - len(chunks))
                if not part:
                    raise EOFError('Daemon closed connection')
                chunks.extend(part)
            return bytes(chunks)
        try:
            for _ in range(500):
                if Path(env['FXI_SOCKET']).exists():
                    break
                if daemon.poll() is not None:
                    raise RuntimeError('Disposable daemon failed to start')
                time.sleep(.01)
            for query, limit in [('foo', 0), ('foo', 150), ('foo top:0', 150),
                ('^99999999999999999999999999999999999999999999999999999:foo', 1),
                ('(' * 10000 + 'foo' + ')' * 10000, 1)]:
                row = {'query_prefix': query[:70], 'query_bytes': len(query.encode()), 'limit': limit}
                try:
                    with socket.socket(socket.AF_UNIX) as stream:
                        stream.settimeout(10)
                        stream.connect(env['FXI_SOCKET'])
                        message = json.dumps({'type': 'Search', 'query': query,
                            'root_path': str(wire_root), 'limit': limit}).encode()
                        stream.sendall(struct.pack('<I', len(message)) + message)
                        size = struct.unpack('<I', receive_exact(stream, 4))[0]
                        response = json.loads(receive_exact(stream, size))
                        row.update(response_type=response['type'],
                            matches=len(response.get('matches', [])),
                            first_match=response.get('matches', [None])[0])
                except (OSError, EOFError) as error:
                    row['error'] = str(error)
                report['wire'].append(row)
            time.sleep(.1)
            report['daemon_exit_after_queries'] = daemon.poll()
        finally:
            if daemon.poll() is None:
                daemon.terminate()
                daemon.wait(timeout=10)
    report['daemon_log'] = log_path.read_text()
    Path(env['FXI_SOCKET']).unlink(missing_ok=True)
print(json.dumps(report, indent=2, ensure_ascii=False))
