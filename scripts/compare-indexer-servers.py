"""Compare complete file results through native server APIs, excluding CLI startup.

Uses indexes and verified corpus coverage from compare-indexers.py. Each request
opens a connection; elapsed time includes wire transfer and JSON decoding. FXI
uses Unix-socket JSON framing; Zoekt uses its stock HTTP JSON API. Payload formats
differ, so this is an API comparison, not an isolated engine microbenchmark.
"""
import argparse
import hashlib
import http.client
import importlib.util
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

spec = importlib.util.spec_from_file_location('indexer_harness', Path(__file__).with_name('compare-indexers.py'))
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class IncompleteSearch(RuntimeError):
    pass


def receive_exact(connection, count):
    data = bytearray()
    while len(data) < count:
        block = connection.recv(count - len(data))
        if not block:
            raise EOFError('Truncated server response')
        data.extend(block)
    return bytes(data)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--benchmark', type=Path, required=True)
    parser.add_argument('--fxi', type=Path)
    parser.add_argument('--zoekt-server', type=Path, default=Path('/tmp/fxi-research-bin/zoekt-webserver'))
    parser.add_argument('--zoekt-path-server', type=Path)
    parser.add_argument('--vary-patterns', action='store_true', help='Append equivalent empty groups to probe syntax-dependent planning')
    parser.add_argument('--repetitions', type=int, default=11)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error('repetitions must be positive')
    source = json.loads(args.benchmark.read_text())
    assert all(source['coverage'][tool]['complete'] for tool in ['fxi', 'zoekt']), 'Incomplete source coverage'
    root, indexes = Path(source['corpus']), Path(source['base'])
    binary = (args.fxi or Path(source['binaries']['fxi']['path'])).resolve()
    runtime = Path(tempfile.mkdtemp(prefix='fxi-indexer-servers-'))
    env = {**os.environ, 'FXI_INDEXES': str(indexes / 'fxi'), 'FXI_SOCKET': str(runtime / 'fxi.sock'),
           'XDG_RUNTIME_DIR': str(runtime)}
    with socket.socket() as available:
        available.bind(('127.0.0.1', 0))
        port = available.getsockname()[1]
    commands = {'fxi': [str(binary), 'daemon', 'foreground'],
                'zoekt': [str(args.zoekt_server.resolve()), '-index', str(indexes / 'zoekt'), '-rpc',
                          '-listen', f'127.0.0.1:{port}']}
    if args.zoekt_path_server:
        commands['zoekt_paths'] = [str(args.zoekt_path_server.resolve()), '-index', str(indexes / 'zoekt'),
                                   '-socket', str(runtime / 'zoekt.sock')]
    servers, logs, rows = {}, {}, []
    def request(tool, pattern):
        start = time.perf_counter_ns()
        if tool in ('fxi', 'zoekt_paths'):
            payload = json.dumps({'type': 'ContentSearch', 'pattern': f're:/{pattern}/' if tool == 'fxi' else pattern, 'root_path': str(root),
                'limit': 0, 'options': {'context_before': 0, 'context_after': 0, 'case_insensitive': False,
                                      'files_only': True, 'compact_files': True}}).encode()
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                connection.settimeout(120)
                connection.connect(env['FXI_SOCKET'] if tool == 'fxi' else str(runtime / 'zoekt.sock'))
                connection.sendall(struct.pack('<I', len(payload)) + payload)
                length = struct.unpack('<I', receive_exact(connection, 4))[0]
                if length > 100 * 1024 * 1024:
                    raise ValueError('Oversize FXI response')
                raw = receive_exact(connection, length)
            reply = json.loads(raw)
            assert reply['type'] == 'ContentSearch', reply
            records = reply['file_paths']
            reported_ms = reply['duration_ms']
        else:
            payload = json.dumps({'Q': helper.zoekt_query(pattern), 'Opts': {
                'ShardMaxMatchCount': 1000000000, 'TotalMaxMatchCount': 1000000000,
                'MaxDocDisplayCount': 0, 'MaxMatchDisplayCount': 0, 'MaxWallTime': 120000000000}})
            connection = http.client.HTTPConnection('127.0.0.1', port, timeout=120)
            try:
                connection.request('POST', '/api/search', payload, {'Content-Type': 'application/json'})
                response = connection.getresponse()
                raw = response.read()
                assert response.status == 200, raw[:1000]
            finally:
                connection.close()
            reply = json.loads(raw)['Result']
            if reply.get('Crashes', 0):
                raise IncompleteSearch(str(reply))
            reported_ms = reply['Duration'] / 1e6
            records = [item['FileName'] for item in reply.get('Files') or []]
        elapsed = (time.perf_counter_ns() - start) / 1e6
        paths = helper.normalize_paths(('\n'.join(records) + ('\n' if records else '')).encode(), root)
        return elapsed, paths, len(raw), reported_ms
    try:
        for tool, command in commands.items():
            logs[tool] = (runtime / f'{tool}.log').open('w')
            servers[tool] = sp.Popen(command, cwd=root, env=env, stdout=logs[tool], stderr=logs[tool])
        deadline = time.monotonic() + 30
        for tool in servers:
            while True:
                assert servers[tool].poll() is None, f'{tool} exited; see {runtime}'
                try:
                    _, paths, _, _ = request(tool, 'auditNonexistentSymbol94283')
                    assert not paths
                    break
                except (OSError, http.client.HTTPException, IncompleteSearch):
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(.05)
        for previous in source['rows']:
            pattern = previous['pattern']
            oracle = sp.run(['rg', '-l', '--color=never', pattern, '.'], cwd=root, capture_output=True, timeout=120)
            assert oracle.returncode in (0, 1), oracle.stderr
            expected = helper.normalize_paths(oracle.stdout, root)
            samples = {tool: [] for tool in servers}
            sizes = {tool: [] for tool in servers}
            reported = {tool: [] for tool in servers}
            for rep in range(-1, args.repetitions):
                order = list(servers)
                random.Random(1729 + rep).shuffle(order)
                for tool in order:
                    assert all(server.poll() is None for server in servers.values()), 'Server exited'
                    elapsed, actual, size, reported_ms = request(tool, pattern + '(?:)' * (rep + 2) if args.vary_patterns else pattern)
                    assert actual == expected, (tool, pattern, sorted(expected - actual)[:10], sorted(actual - expected)[:10])
                    assert all(server.poll() is None for server in servers.values()), 'Server exited'
                    if rep >= 0:
                        samples[tool].append(elapsed)
                        sizes[tool].append(size)
                        reported[tool].append(reported_ms)
            row = {'pattern': pattern, 'files': len(expected), 'tools': {tool: {
                'median_ms': statistics.median(values), 'samples_ms': values,
                'response_bytes': sizes[tool], 'server_reported_ms': reported[tool]} for tool, values in samples.items()}}
            rows.append(row)
            print(pattern, {tool: round(item['median_ms'], 3) for tool, item in row['tools'].items()}, flush=True)
    finally:
        for server in servers.values():
            server.terminate()
        for server in servers.values():
            try:
                server.wait(timeout=10)
            except sp.TimeoutExpired:
                server.kill()
                server.wait()
        for log in logs.values():
            log.close()
    result = {'pattern_variants': args.vary_patterns, 'mode': 'native server API, new connection, includes response transfer and JSON decoding; no CLI startup',
              'source_benchmark': str(args.benchmark.resolve()), 'source_benchmark_sha256': hashlib.sha256(args.benchmark.read_bytes()).hexdigest(),
              'corpus': str(root), 'runtime': str(runtime), 'commands': commands,
              'binaries': {tool: hashlib.sha256(Path(command[0]).read_bytes()).hexdigest() for tool, command in commands.items()},
              'path_adapter_sha256': hashlib.sha256(Path(__file__).with_name('zoekt-path-server').joinpath('main.go').read_bytes()).hexdigest() if args.zoekt_path_server else None,
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'rows': rows}
    args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
