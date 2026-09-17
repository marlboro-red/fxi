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
import sys
import tomllib
from concurrent.futures import ThreadPoolExecutor

harness_sha256 = hashlib.sha256(P.Path(__file__).read_bytes()).hexdigest()
binary_sha256 = None
parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True, type=P.Path)
parser.add_argument('--tool', required=True, choices=['fxi', 'tgrep'])
parser.add_argument('--repetitions', type=int, default=3)
parser.add_argument('--timeout', type=float, default=75)
parser.add_argument('--source', type=P.Path, help='Copy an existing controlled corpus into each isolated fixture')
parser.add_argument('--burst-edits', type=int, default=1)
parser.add_argument('--burst-interval', type=float, default=.025)
parser.add_argument('--poll-interval', type=float, default=.005,
                    help='Delay between completed CLI probes; query time is additional')
parser.add_argument('--debounce-ms', type=int, help='Explicit FXI debounce experiment')
parser.add_argument('--max-batch-age-ms', type=int, help='Explicit FXI maximum event-age experiment')
parser.add_argument('--atomic-save', action='store_true', help='Replace the edited file by atomic rename')
parser.add_argument('--edit-source', type=P.Path, help='Real UTF-8 source payload to save before the marker in the edited probe')
parser.add_argument('--output', required=True, type=P.Path)
args = parser.parse_args()
if (args.repetitions < 1 or args.burst_edits < 1 or args.burst_interval < 0
        or args.timeout <= 0 or args.poll_interval < 0
        or (args.max_batch_age_ms is not None and (args.max_batch_age_ms < 0 or args.tool != 'fxi'))
        or (args.debounce_ms is not None and (args.debounce_ms < 0 or args.tool != 'fxi'))):
    parser.error('Repetitions, edits and timeout must be positive; interval must be nonnegative')
# An index-directory override does not isolate the user config file. Refuse
# hidden watcher tuning rather than silently benchmarking different defaults.
watcher_file_config = {}
if args.tool == 'fxi':
    if sys.platform == 'darwin':
        config_base = P.Path.home() / 'Library' / 'Application Support'
    elif os.name == 'nt':
        config_base = P.Path(os.environ['LOCALAPPDATA'])
    else:
        config_base = P.Path(os.environ.get('XDG_DATA_HOME', P.Path.home() / '.local' / 'share'))
    config_path = config_base / 'fxi' / 'config.toml'
    if config_path.exists():
        watcher_file_config = tomllib.loads(config_path.read_text()).get('watcher', {})
        if watcher_file_config:
            parser.error('User config contains watcher settings; default comparisons require an untuned watcher config')
edit_payload = args.edit_source.read_text() + '\n' if args.edit_source else ''
binary = str(args.binary.resolve())
binary_sha256 = hashlib.sha256(P.Path(binary).read_bytes()).hexdigest()
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
    for key in ['FXI_DELTA_FLUSH_SECS', 'FXI_DEBOUNCE_MS', 'FXI_MAX_BATCH_AGE_MS',
                'FXI_MERGE_SEGMENTS', 'FXI_REBUILD_THRESHOLD']:
        env.pop(key, None)
    if args.debounce_ms is not None:
        env['FXI_DEBOUNCE_MS'] = str(args.debounce_ms)
    if args.max_batch_age_ms is not None:
        env['FXI_MAX_BATCH_AGE_MS'] = str(args.max_batch_age_ms)
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
    def server_rss_kib():
        result = sp.run(['ps', '-o', 'rss=', '-p', str(server.pid)],
                        capture_output=True, check=True, timeout=10)
        return int(result.stdout.strip())
    try:
        time.sleep(1)
        assert 'file_000.rs' in search('symbol_0')
        # FXI registers the watcher during the first query. Give native watcher
        # startup a quiet interval; latency timing starts only at the edit.
        time.sleep(1)
        needle = f'freshnessNeedle{rep}X42'
        assert search(needle) == set()
        warm_rss = server_rss_kib()
        start = time.monotonic()
        (root / 'new.rs').write_text(needle + '\n')
        expected = {'new.rs', 'file_001.rs'}
        def edit_files():
            for edit in range(args.burst_edits - 1):
                (root / 'file_001.rs').write_text(f'intermediate edit {edit}\n')
                time.sleep(args.burst_interval)
            if args.atomic_save:
                (root / '.save.tmp').write_text(edit_payload + needle + '\n')
                (root / '.save.tmp').replace(root / 'file_001.rs')
            else:
                (root / 'file_001.rs').write_text(edit_payload + needle + '\n')
            return time.monotonic()
        first_new_visible = None
        with ThreadPoolExecutor(max_workers=1) as pool:
            editing = pool.submit(edit_files)
            while args.burst_edits > 1 and not editing.done():
                paths = search(needle)
                assert paths <= expected, paths
                if 'new.rs' in paths and first_new_visible is None:
                    first_new_visible = time.monotonic() - start
                time.sleep(args.poll_interval)
            last_edit = editing.result()
        attempts = 0
        last_incomplete_probe = None
        while True:
            probe_start = time.monotonic()
            paths = search(needle)
            attempts += 1
            assert paths <= expected, paths
            elapsed = time.monotonic() - start
            if 'new.rs' in paths and first_new_visible is None:
                first_new_visible = elapsed
            if paths == expected:
                break
            last_incomplete_probe = probe_start - last_edit
            if elapsed > args.timeout:
                raise TimeoutError(f'Not visible after {elapsed:.2f}s: {paths}')
            time.sleep(args.poll_interval)
        after_last_edit = time.monotonic() - last_edit
        visible_rss = server_rss_kib()
        visible_log = log_path.read_text()
        # Preserve the old match while forcing another posting update. A
        # fresh marker proves the daemon loaded that update before we check
        # that old segment entries did not produce duplicate paths.
        retained_marker = f'freshnessRetainedMarker{rep}Z73'
        assert search(retained_marker) == set()
        (root / 'file_001.rs').write_text(edit_payload + needle + '\n' + retained_marker + '\n')
        deadline = time.monotonic() + args.timeout
        while search(retained_marker) != {'file_001.rs'}:
            if time.monotonic() >= deadline:
                raise TimeoutError('Retained-match edit was not indexed')
            time.sleep(args.poll_interval)
        assert search(needle) == expected
        # Deletion/replacement must remove old results as well.
        (root / 'new.rs').unlink()
        (root / 'file_001.rs').write_text('replacement without the needle\n')
        deadline = time.monotonic() + args.timeout
        while search(needle):
            if time.monotonic() >= deadline:
                raise TimeoutError('Stale matches survived removal')
            time.sleep(args.poll_interval)
        persistence_verified = False
        if args.tool == 'fxi':
            # Outside timing: a newly visible marker must survive graceful
            # shutdown and be found by a fresh, direct disk reader.
            persisted = f'persistedFreshnessMarker{rep}Q91'
            (root / 'persist.rs').write_text(persisted + '\n')
            deadline = time.monotonic() + args.timeout
            while search(persisted) != {'persist.rs'}:
                if time.monotonic() >= deadline:
                    raise TimeoutError('Persistence probe never became visible')
                time.sleep(args.poll_interval)
            sp.run([binary, 'daemon', 'stop'], env=env, cwd=root,
                   capture_output=True, check=True, timeout=30)
            server.wait(timeout=30)
            direct = sp.run([binary, '-l', '--color=never', f're:/{persisted}/', '-p', str(root)],
                            env=env, cwd=root, capture_output=True, check=True, timeout=30)
            direct_paths = [P.Path(p).name for p in direct.stdout.decode().splitlines()]
            assert direct_paths == ['persist.rs'], direct
            persistence_verified = True
        row = {'repetition': rep, 'visibility_seconds': elapsed,
               'after_last_edit_seconds': after_last_edit,
               'server_rss_kib': {'before_edits': warm_rss, 'after_visibility': visible_rss},
               'new_file_first_visible_seconds': first_new_visible,
               'graceful_shutdown_persistence_verified': persistence_verified,
               'incremental_publications_before_visibility': visible_log.count('Wrote delta segment ') if args.tool == 'fxi' else None,
               'last_incomplete_probe_started_seconds': last_incomplete_probe,
               'poll_interval_seconds': args.poll_interval, 'attempts': attempts,
               'server_log': str(log_path), 'server_log_text': log_path.read_text()}
        rows.append(row)
        print(args.tool, rep, round(elapsed, 3), 'seconds; exact create/edit/remove results', flush=True)
    finally:
        if server.poll() is None:
            server.terminate()
        try:
            server.wait(timeout=10)
        except sp.TimeoutExpired:
            server.kill()
            server.wait()
args.output.write_text(json.dumps({'tool': args.tool, 'binary': binary,
    'binary_sha256': binary_sha256,
    'harness_sha256': harness_sha256,
    'synthetic_probe_files': 256, 'source': str(args.source.resolve()) if args.source else None,
    'debounce_ms': args.debounce_ms, 'atomic_save': args.atomic_save,
    'edit_source': str(args.edit_source.resolve()) if args.edit_source else None,
    'edit_payload_bytes': len(edit_payload.encode()),
    'edit_payload_sha256': hashlib.sha256(edit_payload.encode()).hexdigest(),
    'max_batch_age_ms': args.max_batch_age_ms,
    'trace_updates': env.get('FXI_TRACE_UPDATES'),
    'watcher_file_config': watcher_file_config,
    'burst_edits': args.burst_edits,
    'burst_interval_seconds': args.burst_interval, 'base': str(base), 'rows': rows}, indent=2))
