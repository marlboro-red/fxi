"""Regression checks for benchmark execution, independent of index timings."""
import json
import importlib.util
import socket
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class StartupHarnessTests(unittest.TestCase):
    def test_alternate_index_is_used_for_timed_samples_and_warmup(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            corpus = base / 'corpus'
            corpus.mkdir()
            (corpus / 'sample.txt').write_text('unrelated content\n')
            log = base / 'calls.jsonl'
            binary = base / 'fake-fxi'
            binary.write_text('#!/usr/bin/env python3\nimport json, os\n'
                              'with open(os.environ["BENCH_TEST_LOG"], "a") as out:\n'
                              '    out.write(json.dumps({k: os.environ.get(k) for k in ["FXI_INDEXES", "RAYON_NUM_THREADS", "FXI_SEARCH_PARALLELISM", "FXI_QUERY_LOCAL", "FXI_GENERATION_ROUTING"]}) + "\\n")\n')
            binary.chmod(0o755)
            before, after = base / 'before', base / 'after'
            command = [sys.executable, str(Path(__file__).with_name('compare-startup.py')),
                       '--corpus', str(corpus), '--indexes', str(before),
                       '--candidate-indexes', str(after), '--baseline', str(binary),
                       '--candidate', str(binary), '--repetitions', '3',
                       '--before-threads', '2', '--after-threads', '8',
                       '--before-search-parallelism', '4', '--after-search-parallelism', '12',
                       '--candidate-query-local', '--candidate-generation-routing',
                       '--patterns', 'definitelyAbsentNeedle', '--output', str(base / 'result.json')]
            subprocess.run(command, env={**os.environ, 'BENCH_TEST_LOG': str(log)},
                           check=True, capture_output=True, timeout=30)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(calls.count({'FXI_INDEXES': str(before.resolve()),
                'RAYON_NUM_THREADS': '2', 'FXI_SEARCH_PARALLELISM': '4',
                'FXI_QUERY_LOCAL': '0', 'FXI_GENERATION_ROUTING': '0'}), 4)
            self.assertEqual(calls.count({'FXI_INDEXES': str(after.resolve()),
                'RAYON_NUM_THREADS': '8', 'FXI_SEARCH_PARALLELISM': '12',
                'FXI_QUERY_LOCAL': '1', 'FXI_GENERATION_ROUTING': '1'}), 4)


def load_harness(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class IndexerHarnessTests(unittest.TestCase):
    def test_publication_policies_are_per_variant_even_with_same_binary(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            corpus = root / 'corpus'
            corpus.mkdir()
            for index in range(16):
                (corpus / f'{index}.rs').write_text('folio_wait_bit_common\n' if index == 0 else 'original\n')
            log = root / 'calls.jsonl'
            binary = root / 'fake-fxi'
            binary.write_text(
                '#!/usr/bin/env python3\n'
                'import json, os, sys\n'
                'from pathlib import Path\n'
                'args = sys.argv[1:]\n'
                "with open(os.environ['BENCH_TEST_LOG'], 'a') as out:\n"
                "    out.write(json.dumps({'args': args, 'query_local': os.environ['FXI_QUERY_LOCAL'], 'generation_routing': os.environ['FXI_GENERATION_ROUTING'], 'stable': os.environ['FXI_STABLE_SEGMENTS']}) + '\\n')\n"
                "if args[0] == 'index':\n"
                "    store = Path(os.environ['FXI_INDEXES']) / 'test-index'\n"
                "    generation = store / 'generations' / 'one'\n"
                '    generation.mkdir(parents=True, exist_ok=True)\n'
                "    (store / 'CURRENT').write_text('one')\n"
                "    added = '--force' not in args\n"
                "    (generation / 'meta.json').write_text(json.dumps({'doc_count': 16 + added, 'segment_count': 1 + added}))\n"
                'else:\n'
                "    print('__fxi_publication_probe_94283.rs' if any('newPublicationMarker' in a for a in args) else '0.rs')\n"
            )
            binary.chmod(0o755)
            output = root / 'report.json'
            subprocess.run([sys.executable, str(Path(__file__).with_name('compare-publication.py')),
                            '--corpus', str(corpus), '--baseline', str(binary), '--candidate', str(binary),
                            '--candidate-query-local', '--candidate-generation-routing',
                            '--repetitions', '1', '--output', str(output)],
                           env={**os.environ, 'BENCH_TEST_LOG': str(log)}, check=True,
                           capture_output=True, timeout=30)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            updates = [call for call in calls if call['args'][0] == 'index' and '--force' not in call['args']]
            self.assertEqual([(call['query_local'], call['generation_routing']) for call in updates],
                             [('0', '0'), ('1', '1')])
            report = json.loads(output.read_text())
            self.assertTrue(report['manifest_verified_before_and_after'])
            self.assertEqual(report['files'], 16)

            log.unlink()
            subprocess.run([sys.executable, str(Path(__file__).with_name('compare-publication.py')),
                            '--corpus', str(corpus), '--baseline', str(binary), '--candidate', str(binary),
                            '--candidate-stable-segments', '--keep-fixture',
                            '--repetitions', '1', '--output', str(output)],
                           env={**os.environ, 'BENCH_TEST_LOG': str(log)}, check=True,
                           capture_output=True, timeout=30)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            builds = [c for c in calls if '--force' in c['args']]
            updates = [c for c in calls if c['args'][0] == 'index' and '--force' not in c['args']]
            self.assertEqual([c['stable'] for c in builds], ['0', '1'])
            self.assertEqual([c['stable'] for c in updates], ['0', '1'])
            report = json.loads(output.read_text())
            fixture, = report['retained_fixtures']
            self.assertNotEqual(fixture['before'], fixture['after'])
            for path in fixture.values():
                self.assertTrue(Path(path).is_dir())
            import shutil
            shutil.rmtree(report['base'])

    def test_focused_open_probe_uses_each_index_and_stops_after_measurement(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / 'probe'
            log = root / 'calls.jsonl'
            binary.write_text('#!/usr/bin/env python3\nimport os, json, time\n'
                              'with open(os.environ["BENCH_TEST_LOG"], "a") as out:\n'
                              '    out.write(json.dumps(os.environ["FXI_INDEXES"]) + "\\n")\n'
                              'print("open_ms\\t1.5", flush=True)\n'
                              'time.sleep(30)\n')
            binary.chmod(0o755)
            indexes = {'before': str(root / 'before'), 'after': str(root / 'after')}
            report = root / 'build.json'
            report.write_text(json.dumps({'corpus': str(root), 'retained_indexes': indexes}))
            output = root / 'result.json'
            subprocess.run([sys.executable, str(Path(__file__).with_name('compare-eager-open.py')),
                            '--build-report', str(report), '--baseline', str(binary),
                            '--candidate', str(binary), '--repetitions', '2', '--output', str(output)],
                           env={**os.environ, 'BENCH_TEST_LOG': str(log)}, check=True,
                           capture_output=True, timeout=10)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(calls.count(indexes['before']), 2)
            self.assertEqual(calls.count(indexes['after']), 2)
            self.assertEqual(json.loads(output.read_text())['samples_ms'],
                             {'before': [1.5, 1.5], 'after': [1.5, 1.5]})

    def test_build_manifest_detects_same_size_source_edits_and_newline_paths(self):
        harness = load_harness('compare-index-builds')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'a.txt').write_text('alpha')
            (root / 'line\nbreak.txt').write_text('beta')
            before = harness.corpus_manifest(root)
            self.assertEqual(before['files'], 2)
            self.assertEqual(before['source_bytes'], 9)
            self.assertEqual(before, harness.corpus_manifest(root))
            (root / 'a.txt').write_text('gamma')
            after = harness.corpus_manifest(root)
            self.assertEqual(before['source_bytes'], after['source_bytes'])
            self.assertNotEqual(before['sha256'], after['sha256'])

    def test_path_normalization_rejects_duplicate_aliases_and_outside_paths(self):
        harness = load_harness('compare-indexers')
        root = Path('/benchmark/corpus')
        self.assertEqual(harness.normalize_paths(b'./a.txt\n/benchmark/corpus/b.txt\n', root), {'a.txt', 'b.txt'})
        with self.assertRaises(ValueError):
            harness.normalize_paths(b'./a.txt\n/benchmark/corpus/a.txt\n', root)
        with self.assertRaises(ValueError):
            harness.normalize_paths(b'/elsewhere/a.txt\n', root)

    def test_framed_response_requires_the_whole_message(self):
        harness = load_harness('compare-indexer-servers')
        reader, writer = socket.socketpair()
        with reader, writer:
            writer.sendall(b'abc')
            writer.shutdown(socket.SHUT_WR)
            self.assertEqual(harness.receive_exact(reader, 3), b'abc')
            with self.assertRaises(EOFError):
                harness.receive_exact(reader, 1)


if __name__ == '__main__':
    unittest.main()
