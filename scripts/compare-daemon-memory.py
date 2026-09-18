"""Compare macOS daemon memory after identical, source-checked API searches.

Each sample starts a private unwatched daemon with checked query-local routing,
warms the same request three times, captures vmmap's summary, and stops it. This
measures a resident workload, not CLI peak RSS or search latency.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import subprocess as sp
import sys
import tempfile
import time


def receive(connection, count):
    chunks = bytearray()
    while len(chunks) < count:
        block = connection.recv(count - len(chunks))
        if not block:
            raise EOFError('Incomplete daemon response')
        chunks.extend(block)
    return bytes(chunks)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--corpus', type=Path, required=True)
    parser.add_argument('--indexes', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--pattern', default='folio_wait_bit_common')
    parser.add_argument('--repetitions', type=int, default=5)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != 'darwin' or args.repetitions < 1:
        parser.error('Requires macOS and positive repetitions')
    root = args.corpus.resolve(strict=True)
    indexes = args.indexes.resolve(strict=True)
    binaries = {'before': args.baseline.resolve(strict=True),
                'after': args.candidate.resolve(strict=True)}
    oracle = sp.run(['rg', '-l', '-0', '--', args.pattern, '.'], cwd=root,
                    capture_output=True, check=False)
    if oracle.returncode not in (0, 1):
        raise RuntimeError(oracle.stderr.decode())
    expected = {path.removeprefix('./') for path in oracle.stdout.decode().split('\0') if path}
    result = {
        'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'corpus': str(root), 'indexes': str(indexes), 'pattern': args.pattern,
        'mode': 'fresh private unwatched daemon; three checked API searches; macOS vmmap summary',
        'query_local': True, 'generation_routing': True, 'source_pack': True,
        'files': len(expected),
        'binaries': {variant: {name: {
            'path': str(binary.parent / name),
            'sha256': hashlib.sha256((binary.parent / name).read_bytes()).hexdigest()
        } for name in ['fxi', 'fxid']} for variant, binary in binaries.items()},
        'samples': {'before': [], 'after': []},
    }

    def query(endpoint):
        payload = json.dumps({
            'type': 'ContentSearch', 'pattern': f're:/{args.pattern}/',
            'root_path': str(root), 'limit': 0,
            'options': {'context_before': 0, 'context_after': 0,
                        'case_insensitive': False, 'files_only': True, 'compact_files': True},
        }).encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(30)
            connection.connect(str(endpoint))
            connection.sendall(struct.pack('<I', len(payload)) + payload)
            length = struct.unpack('<I', receive(connection, 4))[0]
            if length > 100 * 1024 * 1024:
                raise ValueError('Oversize daemon response')
            reply = json.loads(receive(connection, length))
        assert reply['type'] == 'ContentSearch', reply
        records = reply['file_paths']
        actual = {str(Path(path).relative_to(root)) if Path(path).is_absolute()
                  else path.removeprefix('./') for path in records}
        assert len(records) == len(actual) and actual == expected, reply

    for repetition in range(args.repetitions):
        for variant in (['before', 'after'] if repetition % 2 == 0 else ['after', 'before']):
            # Keep socket paths short enough for sockaddr_un on macOS.
            with tempfile.TemporaryDirectory(prefix='fxi-memory-', dir='/tmp') as temporary:
                runtime = Path(temporary)
                endpoint = runtime / 's.sock'
                env = dict(os.environ, FXI_INDEXES=str(indexes), FXI_SOCKET=str(endpoint),
                           XDG_RUNTIME_DIR=str(runtime), FXI_APP_DATA=str(runtime / 'app'),
                           FXI_QUERY_LOCAL='1', FXI_GENERATION_ROUTING='1',
                           FXI_NEGATIVE_ROUTING='0', FXI_SOURCE_PACK='1')
                with (runtime / 'daemon.log').open('w+') as log:
                    command = [str(binaries[variant]), 'daemon', 'foreground']
                    process = sp.Popen(command, env=env, stdout=log, stderr=log)
                    try:
                        for attempt in range(200):
                            if process.poll() is not None:
                                log.seek(0)
                                raise RuntimeError(log.read())
                            try:
                                query(endpoint)
                                break
                            except (FileNotFoundError, ConnectionRefusedError):
                                time.sleep(0.05)
                        else:
                            raise TimeoutError('Daemon did not become ready')
                        query(endpoint)
                        query(endpoint)
                        summary = sp.check_output(
                            ['/usr/bin/vmmap', '-summary', str(process.pid)], text=True,
                            stderr=sp.STDOUT, timeout=30)
                        result['samples'][variant].append({
                            'pid': process.pid, 'command': command, 'vmmap_summary': summary})
                        print(variant, [line for line in summary.splitlines()
                                        if 'footprint' in line.lower()], flush=True)
                    finally:
                        if process.poll() is None:
                            process.terminate()
                            try:
                                process.wait(timeout=15)
                            except sp.TimeoutExpired:
                                process.kill()
                                process.wait()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
