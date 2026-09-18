"""Compare two FXI daemons using one immutable index; validate every sample."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import statistics
import socket
import struct
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--corpus', type=Path, required=True)
parser.add_argument('--indexes', type=Path, required=True)
parser.add_argument('--baseline', type=Path, required=True)
parser.add_argument('--candidate', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--repetitions', type=int, default=11)
parser.add_argument('--before-search-parallelism', type=int)
parser.add_argument('--after-search-parallelism', type=int)
parser.add_argument('--patterns', nargs='+')
parser.add_argument('--literal', action='store_true', help='Compare bare identifiers with case-insensitive fixed-string ripgrep')
args = parser.parse_args()
if args.repetitions < 1:
    parser.error('repetitions must be positive')
search_tasks = {'before': args.before_search_parallelism, 'after': args.after_search_parallelism}
if any(value is not None and value < 1 for value in search_tasks.values()):
    parser.error('Search parallelism must be positive')
patterns = args.patterns or ['folio_wait_bit_common', 'auditNonexistentSymbol94283', 'struct file_operations', 'return']
if args.literal and any(not pattern.isidentifier() for pattern in patterns):
    parser.error('--literal requires identifier patterns to preserve bare-query semantics')
root = args.corpus.resolve()
base = Path(tempfile.mkdtemp(prefix='fxi-warm-files-'))
binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
servers, logs, envs = {}, {}, {}
rows = []

def paths(output):
    result = []
    for line in output.decode().splitlines():
        path = Path(line)
        result.append(str(path.relative_to(root)) if path.is_absolute() else line.removeprefix('./'))
    assert len(result) == len(set(result)), 'Duplicate paths'
    return set(result)

try:
    for name, binary in binaries.items():
        runtime = base / name
        runtime.mkdir()
        envs[name] = {**os.environ, 'FXI_INDEXES': str(args.indexes.resolve()),
                      'FXI_SOCKET': str(runtime / 'fxi.sock'), 'XDG_RUNTIME_DIR': str(runtime)}
        if search_tasks[name] is not None:
            envs[name]['FXI_SEARCH_PARALLELISM'] = str(search_tasks[name])
        logs[name] = (runtime / 'server.log').open('w')
        servers[name] = subprocess.Popen([str(binary), 'daemon', 'foreground'], cwd=root,
                                        env=envs[name], stdout=logs[name], stderr=logs[name])
    deadline = time.monotonic() + 20
    while not all(Path(env['FXI_SOCKET']).exists() for env in envs.values()):
        assert all(server.poll() is None for server in servers.values()), 'Daemon exited'
        if time.monotonic() > deadline:
            raise TimeoutError('Daemon socket startup')
        time.sleep(.02)
    for pattern, mode in [(p,m) for p in patterns for m in ['regex','phrase']]:
        oracle = subprocess.run(['rg', *(['-i', '-F'] if args.literal else []), '-l', '--color=never', pattern, '.'], cwd=root, capture_output=True, timeout=120)
        assert oracle.returncode in (0, 1), oracle.stderr
        expected = paths(oracle.stdout)
        samples = {name: [] for name in binaries}
        first_query_ms = {}
        for rep in range(-1, args.repetitions):
            order = list(binaries)
            random.Random(1729 + rep).shuffle(order)
            for name in order:
                assert servers[name].poll() is None
                variant = json.dumps(pattern) if mode == 'phrase' else f"re:/{pattern}/"
                payload = json.dumps({'type':'ContentSearch', 'pattern':variant, 'root_path':str(root), 'limit':0, 'options':{'files_only':True,'compact_files':True,'case_insensitive':False,'context_before':0,'context_after':0}}).encode()
                def exact(connection, n):
                    data = bytearray()
                    while len(data) < n:
                        block = connection.recv(n-len(data))
                        if not block: raise EOFError('truncated response')
                        data.extend(block)
                    return data
                start = time.perf_counter_ns()
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                    connection.settimeout(120)
                    connection.connect(envs[name]['FXI_SOCKET'])
                    connection.sendall(struct.pack('<I',len(payload))+payload)
                    length=struct.unpack('<I',exact(connection,4))[0]
                    assert length < 100*1024*1024
                    reply=json.loads(exact(connection,length))
                elapsed=(time.perf_counter_ns()-start)/1e6
                assert reply['type']=='ContentSearch', reply
                actual=paths(('\n'.join(reply['file_paths'])+'\n').encode() if reply['file_paths'] else b'')
                assert actual==expected,(name,pattern,len(actual),len(expected))
                if rep >= 0:
                    samples[name].append(elapsed)
                else:
                    first_query_ms[name] = elapsed
        row = {'pattern': pattern, 'mode': mode, 'files': len(expected), 'first_query_ms': first_query_ms,
               'server_rss_kib': {name: int(subprocess.check_output(['ps', '-o', 'rss=', '-p', str(server.pid)], text=True).strip()) for name, server in servers.items()}, 'tools': {
            name: {'median_ms': statistics.median(values), 'samples_ms': values}
            for name, values in samples.items()}}
        rows.append(row)
        print(mode, pattern, {name: entry['median_ms'] for name, entry in row['tools'].items()}, flush=True)
finally:
    for server in servers.values():
        server.terminate()
    for server in servers.values():
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()
    for log in logs.values():
        log.close()
args.output.write_text(json.dumps({'search_parallelism': search_tasks, 'experiment_environment': {key: os.environ.get(key) for key in ['FXI_SOURCE_PACK', 'FXI_INTERIOR_TOKENS']}, 'query_syntax': 'case-sensitive regex and quoted phrase; fixed queries', 'base': str(base), 'corpus': str(root), 'indexes': str(args.indexes.resolve()),
    'binaries': {name: {'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
                 for name, binary in binaries.items()},
    'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'rows': rows}, indent=2) + '\n')
