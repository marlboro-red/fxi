"""Materialize exact eligible Git blobs, including case-colliding source paths.

The public checkout can be shallow. Do not trust a case-insensitive working tree
for a case-sensitive source repository. Collision paths are renamed explicitly
and recorded, never silently replaced. Manifest lives outside the search corpus.
"""
import argparse
import hashlib
import json
import pathlib
import subprocess
from collections import Counter

parser = argparse.ArgumentParser()
parser.add_argument('source', type=pathlib.Path)
parser.add_argument('destination', type=pathlib.Path)
args = parser.parse_args()
args.destination.mkdir(parents=True, exist_ok=False)
revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=args.source, text=True).strip()
entries = []
for record in subprocess.check_output(['git', 'ls-tree', '-rz', 'HEAD'], cwd=args.source).split(b'\0'):
    if not record:
        continue
    header, raw_name = record.split(b'\t', 1)
    mode, kind, oid = header.split()
    name = raw_name.decode('utf-8')
    path = pathlib.PurePosixPath(name)
    if mode not in (b'100644', b'100755') or kind != b'blob':
        continue
    if path.suffix not in {'.c', '.h', '.tcl', '.py', '.md', '.rs'}:
        continue
    if any(part.startswith('.') or part in {'target', 'node_modules', 'venv', '__pycache__'} for part in path.parts):
        continue
    entries.append((name, oid))
counts = Counter(name.casefold() for name, _ in entries)
manifest = []
process = subprocess.Popen(['git', 'cat-file', '--batch'], cwd=args.source, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
try:
    for name, oid in entries:
        process.stdin.write(oid + b'\n')
        process.stdin.flush()
        header = process.stdout.readline().split()
        assert header[0] == oid and header[1] == b'blob', header
        size = int(header[2])
        data = process.stdout.read(size)
        assert len(data) == size and process.stdout.read(1) == b'\n'
        if not data or size > 10_000_000 or b'\0' in data:
            continue
        try:
            data.decode('utf-8')
        except UnicodeDecodeError:
            continue
        target = pathlib.PurePosixPath(name)
        if counts[name.casefold()] > 1:
            target = target.with_name('fxi_collision_' + hashlib.sha256(name.encode()).hexdigest()[:12] + '_' + target.name)
        destination = args.destination / target
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open('xb') as output:
            output.write(data)
        manifest.append({'source': name, 'materialized': str(target), 'blob': oid.decode(), 'bytes': size,
                         'sha256': hashlib.sha256(data).hexdigest()})
finally:
    process.stdin.close()
    process.wait()
assert process.returncode == 0
subprocess.run(['git', 'init', '-q', str(args.destination)], check=True)
result = {'source_revision': revision, 'source': str(args.source), 'files': manifest,
          'file_count': len(manifest), 'bytes': sum(row['bytes'] for row in manifest),
          'renamed_collisions': [row for row in manifest if row['source'] != row['materialized']]}
encoded = json.dumps(result, indent=2).encode()
manifest_path = args.destination.with_suffix('.manifest.json')
manifest_path.write_bytes(encoded)
print(json.dumps({'corpus': str(args.destination), 'manifest': str(manifest_path),
                  'manifest_sha256': hashlib.sha256(encoded).hexdigest(),
                  'revision': revision, 'files': result['file_count'], 'bytes': result['bytes'],
                  'renamed_collisions': len(result['renamed_collisions'])}, indent=2))
