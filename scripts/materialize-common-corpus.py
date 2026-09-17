"""Copy the explicitly reported csearch-supported subset; preserve exclusions.

This does not replace the full-corpus comparison or erase its coverage failures.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument('--benchmark', required=True, type=Path)
parser.add_argument('--output', required=True, type=Path)
parser.add_argument('--manifest', required=True, type=Path)
args = parser.parse_args()
source = json.loads(args.benchmark.read_text())
root = Path(source['corpus'])
coverage = source.get('corrected_csearch_coverage', source['coverage']['csearch'])
assert not coverage.get('probe_invalid') and not coverage.get('missing_list_truncated')
assert not coverage['extra'], 'Unexpected indexed paths need investigation'
excluded = set(coverage['missing'])
paths = subprocess.check_output(['rg', '--files'], cwd=root, text=True).splitlines()
assert excluded <= set(paths)
assert len(paths) - len(excluded) == coverage['indexed_files']
args.output.mkdir(parents=True, exist_ok=False)
for name in paths:
    if name in excluded:
        continue
    original = root / name
    assert not original.is_symlink(), 'Controlled fixture must contain regular files'
    destination = args.output / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(original, destination)
args.manifest.write_text(json.dumps({'source': str(root), 'corpus': str(args.output.resolve()),
    'excluded': sorted(excluded), 'reason': 'Files omitted by unmodified csearch. Full-corpus coverage failure remains reported separately.'}, indent=2) + '\n')
