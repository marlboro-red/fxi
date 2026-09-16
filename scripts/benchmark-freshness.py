"""Measure save-to-search visibility through real native watchers and CLI clients.

Synthetic fixture, separate roots/indexes per sample, strict results and FXI
fallback rejection. Includes debounce/reconciliation time, not just query time.
"""
import argparse
import hashlib
import json
import os
import pathlib as P
import shutil
import subprocess as sp
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True, type=P.Path)
parser.add_argument('--tool', required=True, choices=['fxi', 'tgrep'])
parser.add_argument('--repetitions', type=int, default=3)
parser.add_argument('--timeout', type=float, default=75)
parser.add_argument('--source', type=P.Path, help='Copy an existing controlled corpus into each isolated fixture')
parser.add_argument('--burst-edits', type=int, default=1)
parser.add_argument('--burst-interval', type=float, default=.025)
parser.add_argument('--output', required=True, type=P.Path)
args = parser.parse_args()
if args.repetitions < 1 or args.burst_edits < 1 or args.burst_interval < 0 or args.timeout <= 0:
    parser.error('Repetitions, edits and timeout must be positive; interval must be nonnegative')
binary = str(args.binary.resolve())
base = P.Path(tempfile.mkdtemp(prefix='fxi-freshness-'))
rows = []
for rep in range(args.repetitions):
    work = base / str(rep)
    root = work / 'corpus'
    root.mkdir(parents=True)
    if args.source:
        shutil.copytree(args.source.resolve(), root, dirs_exist_ok=True, ignore=shutil.ignore_patterns('.git', '.tgrep'))
    sp.run(['git', 'init', '-q', str(root)], check=True)
    for i in range(256):
        assert not (root / f'file_{i:03}.rs').exists(), 'Synthetic probe path collides with source'
        (root / f'file_{i:03}.rs').write_text(f'fn symbol_{i}() {{}}\n')
    runtime = work / 'runtime'
    runtime.mkdir()
    env = {**os.environ, 'FXI_INDEXES': str(work / 'indexes'),
           'FXI_SOCKET': str(runtime / 'fxi.sock'), 'XDG_RUNTIME_DIR': str(runtime)}
    # Exercise the binary's defaults rather than inherited tuning variables.
    for key in ['FXI_DELTA_FLUSH_SECS', 'FXI_DEBOUNCE_MS']:
        env.pop(key, None)
    sp.run([binary, 'index', '--force', str(root)], env=env, cwd=root,
           capture_output=True, check=True, timeout=120)
    command = ([binary, 'daemon', 'foreground', '--watch'] if args.tool == 'fxi'
               else [binary, 'serve', str(root)])
    log_path = work / 'server.log'
    with log_path.open('w') as log:
        server = sp.Popen(command, env=env, cwd=root, stdout=log, stderr=log)
    def search(pattern):
        assert server.poll() is None, 'Server exited'
        command = ([binary, '-l', '--color=never', f're:/{pattern}/', '-p', str(root)]
                   if args.tool == 'fxi' else [binary, '-l', '--color=never', pattern, str(root)])
        result = sp.run(command, env=env, cwd=root, capture_output=True, timeout=10)
        valid = (0,) if args.tool == 'fxi' else (0, 1)
        if result.returncode not in valid or b'Daemon search failed' in result.stderr:
            raise RuntimeError((result.returncode, result.stderr.decode()))
        paths = [str(P.Path(p).relative_to(root)) if P.Path(p).is_absolute() else p.removeprefix('./')
                 for p in result.stdout.decode().splitlines()]
        assert len(paths) == len(set(paths)), 'Duplicate paths'
        return set(paths)
    try:
        time.sleep(1)
        assert 'file_000.rs' in search('symbol_0')
        # FXI registers the watcher during the first query. Give native watcher
        # startup a quiet interval; latency timing starts only at the edit.
        time.sleep(1)
        needle = f'freshnessNeedle{rep}X42'
        assert search(needle) == set()
        start = time.monotonic()
        (root / 'new.rs').write_text(needle + '\n')
        for edit in range(args.burst_edits - 1):
            (root / 'file_001.rs').write_text(f'intermediate edit {edit}\n')
            time.sleep(args.burst_interval)
        (root / 'file_001.rs').write_text(needle + '\n')
        last_edit = time.monotonic()
        expected = {'new.rs', 'file_001.rs'}
        attempts = 0
        while True:
            paths = search(needle)
            attempts += 1
            assert paths <= expected, paths
            elapsed = time.monotonic() - start
            if paths == expected:
                break
            if elapsed > args.timeout:
                raise TimeoutError(f'Not visible after {elapsed:.2f}s: {paths}')
            time.sleep(.1)
        visible_log = log_path.read_text()
        after_last_edit = time.monotonic() - last_edit
        # Preserve the old match while forcing another posting update. A
        # fresh marker proves the daemon loaded that update before we check
        # that old segment entries did not produce duplicate paths.
        retained_marker = f'freshnessRetainedMarker{rep}Z73'
        assert search(retained_marker) == set()
        (root / 'file_001.rs').write_text(needle + '\n' + retained_marker + '\n')
        deadline = time.monotonic() + args.timeout
        while search(retained_marker) != {'file_001.rs'}:
            if time.monotonic() >= deadline:
                raise TimeoutError('Retained-match edit was not indexed')
            time.sleep(.1)
        assert search(needle) == expected
        # Deletion/replacement must remove old results as well.
        (root / 'new.rs').unlink()
        (root / 'file_001.rs').write_text('replacement without the needle\n')
        deadline = time.monotonic() + args.timeout
        while search(needle):
            if time.monotonic() >= deadline:
                raise TimeoutError('Stale matches survived removal')
            time.sleep(.1)
        row = {'repetition': rep, 'visibility_seconds': elapsed,
               'after_last_edit_seconds': after_last_edit,
               'incremental_publications_before_visibility': visible_log.count('Performing incremental update...') if args.tool == 'fxi' else None,
               'poll_interval_seconds': .1, 'attempts': attempts,
               'server_log': str(log_path), 'server_log_text': log_path.read_text()}
        rows.append(row)
        print(args.tool, rep, round(elapsed, 3), 'seconds; exact create/edit/remove results', flush=True)
    finally:
        server.terminate()
        try:
            server.wait(timeout=10)
        except sp.TimeoutExpired:
            server.kill()
            server.wait()
args.output.write_text(json.dumps({'tool': args.tool, 'binary': binary,
    'binary_sha256': hashlib.sha256(P.Path(binary).read_bytes()).hexdigest(),
    'harness_sha256': hashlib.sha256(P.Path(__file__).read_bytes()).hexdigest(),
    'synthetic_probe_files': 256, 'source': str(args.source.resolve()) if args.source else None,
    'burst_edits': args.burst_edits,
    'burst_interval_seconds': args.burst_interval, 'base': str(base), 'rows': rows}, indent=2))
