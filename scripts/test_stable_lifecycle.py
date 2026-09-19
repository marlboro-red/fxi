"""Harness checks only; these never execute or compile the real FXI binary."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    'stable_lifecycle', Path(__file__).with_name('validate-stable-lifecycle.py'))
HARNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HARNESS)


FAKE = r'''#!/usr/bin/env python3
import json, os, re, shutil, sys
from pathlib import Path
args = sys.argv[1:]
root = Path.cwd()
env_keys = ['FXI_APP_DATA', 'FXI_INDEXES', 'FXI_SOCKET', 'XDG_RUNTIME_DIR',
            'FXI_STABLE_SEGMENTS', 'FXI_QUERY_LOCAL', 'FXI_NEGATIVE_ROUTING',
            'FXI_GENERATION_ROUTING', 'FXI_SOURCE_PACK']
with open(os.environ['LIFECYCLE_TEST_LOG'], 'a') as out:
    out.write(json.dumps({'args': args, 'env': {k: os.environ.get(k) for k in env_keys}}) + '\n')
store = Path(os.environ['FXI_INDEXES']) / 'fake'
state_path = Path(os.environ['FXI_APP_DATA']) / 'fake-state.json'
sources = {p.relative_to(root).as_posix(): p.read_bytes().decode().splitlines()
           for p in sorted(root.rglob('*.txt'))}
if args[0] in ('index', 'compact'):
    state = json.loads(state_path.read_text()) if state_path.exists() else {'sequence': 0, 'profile': args[args.index('--profile') + 1]}
    state['sequence'] += 1
    name = 'gen-fake-' + str(state['sequence'])
    previous = (store / 'CURRENT').read_text() if (store / 'CURRENT').exists() else None
    mapping = {}
    if previous and args[0] != 'compact':
        mapping = json.loads((store / 'generations' / previous / 'meta.json').read_text())['segment_objects']
    identifier = str(max(map(int, mapping), default=0) + 1)
    object_name = name + '-seg-' + identifier
    mapping[identifier] = object_name
    obj = store / 'objects' / object_name
    obj.mkdir(parents=True)
    (obj / 'grams.postings').write_bytes(b'fake postings')
    generation = store / 'generations' / name
    generation.mkdir(parents=True)
    ids = sorted(map(int, mapping))
    meta = {'version': 4 if state['profile'] == 'full' else 5,
            'base_segment': ids[0], 'delta_segments': ids[1:], 'segment_count': len(ids),
            'segment_objects': mapping, 'doc_count': len(sources),
            'valid_doc_count': len(sources), 'tombstone_count': 0}
    (generation / 'meta.json').write_text(json.dumps(meta))
    (generation / 'objects.check').write_bytes(b'fake binding')
    (store / 'CURRENT').write_text(name)
    state_path.write_text(json.dumps(state))
    for old in (store / 'generations').iterdir():
        if old.name not in (name, previous):
            shutil.rmtree(old)
    live = set()
    for retained in (store / 'generations').iterdir():
        live.update(json.loads((retained / 'meta.json').read_text())['segment_objects'].values())
    for old in (store / 'objects').iterdir():
        if old.name not in live and os.environ.get('LIFECYCLE_TEST_LEAK') != '1':
            shutil.rmtree(old)
else:
    pattern = args[args.index('-e') + 1]
    expression = re.compile(pattern if '--regex' in args else re.escape(pattern), re.I if '-i' in args else 0)
    rows = []
    for path, lines in sources.items():
        for number, line in enumerate(lines, 1):
            match = expression.search(line)
            if match:
                rows.append({'path': path, 'line_number': number, 'line_content': line,
                             'match_start': len(line[:match.start()].encode()),
                             'match_end': len(line[:match.end()].encode()),
                             'context_before': [], 'context_after': []})
    if os.environ.get('LIFECYCLE_TEST_OMIT') == '1':
        rows = rows[1:]
    counts = {}
    for row in rows:
        counts[row['path']] = counts.get(row['path'], 0) + 1
    print(json.dumps({'matches': rows, 'file_paths': sorted(counts),
                      'file_counts': sorted(counts.items()), 'files_with_matches': len(counts)}))
'''


class SourceOracleTests(unittest.TestCase):
    def test_oracle_checks_utf8_offsets_crlf_and_missing_final_newline(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'café.txt').write_bytes('🦀 alpha\r\nfoo/bar\r\nALPHA'.encode())
            sources = HARNESS.read_sources(root)
            rows = HARNESS.source_matches(sources, 'alpha', insensitive=True)
            self.assertEqual([(r['line_number'], r['match_start'], r['match_end']) for r in rows],
                             [(1, 5, 10), (3, 0, 5)])
            self.assertEqual([r['line_content'] for r in rows], ['🦀 alpha', 'ALPHA'])
            self.assertEqual(HARNESS.source_matches(sources, 'foo[/.]bar', 'regex')[0]['line_number'], 2)
            self.assertEqual(HARNESS.source_matches(sources, 'absent'), [])

    def test_membership_duplicates_and_wrong_content_cannot_pass(self):
        root = Path('/private/corpus')
        expected = HARNESS.source_matches({'a.txt': ['alpha']}, 'alpha')
        with self.assertRaises(AssertionError):
            HARNESS.verify_response({'files_with_matches': 1, 'file_paths': ['a.txt', './a.txt']},
                                    expected, root, 'files')
        with self.assertRaises(AssertionError):
            HARNESS.verify_response({'files_with_matches': 1, 'matches': [dict(expected[0], match_start=1)]},
                                    expected, root, 'content')
        with self.assertRaises(AssertionError):
            HARNESS.normalize_path('../outside.txt', root)

    def test_seeded_corpus_is_balanced_bounded_and_reproducible(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            first = HARNESS.Corpus(base / 'first', 42, 16)
            second = HARNESS.Corpus(base / 'second', 42, 16)
            operations = []
            for _ in range(40):
                operation = first.mutate()
                self.assertEqual(operation, second.mutate())
                operations.append(operation['kind'])
                self.assertLessEqual(abs(len(first.paths) - 16), 1)
            self.assertEqual({kind: operations.count(kind) for kind in set(operations)},
                             dict(edit=10, add=10, delete=10, rename=10))
            self.assertEqual(HARNESS.read_sources(first.root), HARNESS.read_sources(second.root))


@unittest.skipUnless(os.name == 'posix', 'The fake executable uses a Unix shebang')
class FakeCampaignTests(unittest.TestCase):
    def run_fake(self, omit=False, leak=False):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            binary = base / 'fake-fxi'
            binary.write_text(FAKE)
            binary.chmod(0o755)
            output, log = base / 'report.json', base / 'calls.jsonl'
            external = base / 'must-not-touch'
            external.mkdir()
            with patch.dict(os.environ, {'LIFECYCLE_TEST_LOG': str(log),
                                         'LIFECYCLE_TEST_OMIT': '1' if omit else '0',
                                         'LIFECYCLE_TEST_LEAK': '1' if leak else '0',
                                         'FXI_INDEXES': str(external), 'FXI_APP_DATA': str(external),
                                         'FXI_SOCKET': str(external / 'real.sock'),
                                         'FXI_QUERY_LOCAL': '1', 'FXI_GENERATION_ROUTING': '1'}):
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    code = HARNESS.main(['--binary', str(binary), '--output', str(output),
                                         '--steps', '4', '--files', '16', '--compact-every', '4',
                                         '--audit-every', '4', '--checkpoint-every', '4'])
            report = json.loads(output.read_text())
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(list(external.iterdir()), [])
            self.assertTrue(report['workspace_cleaned'])
            self.assertFalse(Path(report['temporary_workspace']).exists())
            self.assertEqual(report['binary']['sha256'], HARNESS.sha256(binary))
            self.assertTrue(all(sample['elapsed_ns'] > 0 for sample in report['samples']))
            for call in calls:
                env = call['env']
                for name in ('FXI_APP_DATA', 'FXI_INDEXES', 'FXI_SOCKET', 'XDG_RUNTIME_DIR'):
                    self.assertTrue(Path(env[name]).is_relative_to(report['temporary_workspace']))
                self.assertEqual([env[k] for k in ('FXI_STABLE_SEGMENTS', 'FXI_QUERY_LOCAL',
                                                   'FXI_NEGATIVE_ROUTING', 'FXI_GENERATION_ROUTING')],
                                 ['1', '0', '0', '0'])
            return code, report

    def test_both_profiles_raw_samples_storage_and_cleanup(self):
        code, report = self.run_fake()
        self.assertEqual(code, 0, report.get('traceback'))
        self.assertEqual(report['status'], 'passed')
        self.assertEqual(report['checks']['reclamations'], 2)
        self.assertGreater(report['checks']['queries'], 40)
        self.assertEqual([r['profile'] for r in report['profiles']], ['full', 'lean'])
        self.assertEqual(report['profiles'][0]['workload_sha256'], report['profiles'][1]['workload_sha256'])
        for profile in report['profiles']:
            self.assertEqual(profile['operation_counts'], dict(add=1, delete=1, edit=1, rename=1))
            self.assertEqual(profile['final_storage']['noncurrent_object_count'], 0)
            self.assertGreater(profile['reclaimed_object_count'], 0)

    def test_false_negative_fails_and_still_cleans_private_workspace(self):
        code, report = self.run_fake(omit=True)
        self.assertEqual(code, 1)
        self.assertEqual(report['status'], 'failed')
        self.assertIn('Content differs', report['failure'])
        self.assertIn('actual', report['failure_query'])

    def test_unreclaimed_objects_fail_the_final_reachability_check(self):
        code, report = self.run_fake(leak=True)
        self.assertEqual(code, 1)
        self.assertEqual(report['status'], 'failed')
        self.assertIn('Retired segment objects survived final publication', report['failure'])


if __name__ == '__main__':
    unittest.main()
