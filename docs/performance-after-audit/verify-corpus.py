"""Independently verify the reused corpus bytes against the recorded snapshot."""
import hashlib, json, pathlib, subprocess, sys
snapshot = json.loads(pathlib.Path(sys.argv[1]).read_text())
root = pathlib.Path(snapshot['corpus'])
paths = sorted(subprocess.check_output(['rg', '--files'], cwd=root).decode().splitlines())
manifest = hashlib.sha256(); total = 0
for name in paths:
    data = (root/name).read_bytes(); total += len(data)
    manifest.update(json.dumps([name, hashlib.sha256(data).hexdigest()], separators=(',', ':')).encode()+b'\n')
actual = manifest.hexdigest()
assert actual == snapshot['manifest_sha256'], (actual, snapshot['manifest_sha256'])
assert len(paths) == snapshot['files']
pathlib.Path(sys.argv[2]).write_text(json.dumps({'files':len(paths),'bytes':total,'verified_manifest_sha256':actual},indent=2)+'\n')
print('Verified corpus bytes:',len(paths),total,actual)
