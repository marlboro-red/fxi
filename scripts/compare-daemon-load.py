"""Measure fresh-daemon index load and warm queries against a ripgrep oracle.

Every sample starts a new daemon. Socket binding/process startup are excluded;
the first request includes index opening. Filesystem caches are not flushed.
No watcher is started. RSS is sampled after first request and warm repeats.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import socket
import statistics
import struct
import subprocess as sp
import tempfile
import time


def receive(connection, count):
    result = bytearray()
    while len(result) < count:
        block = connection.recv(count - len(result))
        if not block:
            raise EOFError('Truncated response')
        result.extend(block)
    return result


def request(address, root, pattern):
    payload = json.dumps({'type': 'ContentSearch', 'pattern': f're:/{pattern}/',
        'root_path': str(root), 'limit': 0, 'options': {
            'context_before': 0, 'context_after': 0, 'case_insensitive': False,
            'files_only': True, 'compact_files': True}}).encode()
    start = time.perf_counter_ns()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(120)
        connection.connect(str(address))
        connection.sendall(struct.pack('<I', len(payload)) + payload)
        length = struct.unpack('<I', receive(connection, 4))[0]
        assert length <= 100 * 1024 * 1024, length
        response = json.loads(receive(connection, length))
    elapsed = (time.perf_counter_ns() - start) / 1e6
    assert response['type'] == 'ContentSearch', response
    paths = response['file_paths']
    assert len(paths) == len(set(paths)), 'Duplicate paths'
    return elapsed, set(paths)


def rss(pid):
    return int(sp.check_output(['ps', '-o', 'rss=', '-p', str(pid)], text=True))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--corpus', type=Path, required=True)
    parser.add_argument('--indexes', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--patterns', nargs='+', default=[
        'auditNonexistentSymbol94283', 'struct file_operations', 'return'])
    parser.add_argument('--repetitions', type=int, default=7)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error('repetitions must be positive')
    root = args.corpus.resolve()
    binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
    base = Path(tempfile.mkdtemp(prefix='fxi-daemon-load-'))
    rows = []
    for pattern_index, pattern in enumerate(args.patterns):
        oracle = sp.run(['rg', '-l', '-0', '--color=never', '--', pattern, '.'],
                        cwd=root, capture_output=True, timeout=120)
        assert oracle.returncode in (0, 1), oracle.stderr
        expected = {p.removeprefix('./') for p in oracle.stdout.decode().split('\0') if p}
        samples = {name: [] for name in binaries}
        for repetition in range(args.repetitions):
            order = list(binaries)
            random.Random(613 + repetition).shuffle(order)
            for name in order:
                runtime = base / f'{pattern_index}-{repetition}-{name}'
                runtime.mkdir()
                address = runtime / 'fxi.sock'
                env = {**os.environ, 'FXI_INDEXES': str(args.indexes.resolve()),
                       'FXI_SOCKET': str(address), 'XDG_RUNTIME_DIR': str(runtime)}
                log_path = runtime / 'server.log'
                with log_path.open('w') as log:
                    server = sp.Popen([str(binaries[name]), 'daemon', 'foreground'],
                                      env=env, cwd=root, stdout=log, stderr=log)
                try:
                    deadline = time.monotonic() + 20
                    while not address.exists():
                        assert server.poll() is None, log_path.read_text()
                        if time.monotonic() > deadline:
                            raise TimeoutError('Daemon startup')
                        time.sleep(.01)
                    first, actual = request(address, root, pattern)
                    assert actual == expected, (name, pattern, actual ^ expected)
                    first_rss = rss(server.pid)
                    warm = []
                    for _ in range(5):
                        elapsed, actual = request(address, root, pattern)
                        assert actual == expected, (name, pattern, actual ^ expected)
                        warm.append(elapsed)
                    samples[name].append({'first_ms': first, 'first_rss_kib': first_rss,
                        'warm_ms': warm, 'warm_rss_kib': rss(server.pid)})
                    print(pattern, repetition, name, round(first, 3), 'ms', flush=True)
                finally:
                    server.terminate()
                    try:
                        server.wait(timeout=10)
                    except sp.TimeoutExpired:
                        server.kill()
                        server.wait()
        rows.append({'pattern': pattern, 'files': len(expected), 'tools': {
            name: {'samples': values,
                   'median_first_ms': statistics.median(v['first_ms'] for v in values),
                   'median_first_rss_kib': statistics.median(v['first_rss_kib'] for v in values),
                   'median_warm_ms': statistics.median(t for v in values for t in v['warm_ms'])}
            for name, values in samples.items()}})
    args.output.write_text(json.dumps({'corpus': str(root), 'indexes': str(args.indexes.resolve()),
        'runtime': str(base), 'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'environment': {key: os.environ.get(key) for key in ['FXI_SOURCE_PACK', 'FXI_SEARCH_PARALLELISM']},
        'binaries': {name: {'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
                     for name, binary in binaries.items()}, 'rows': rows}, indent=2) + '\n')


if __name__ == '__main__':
    main()
