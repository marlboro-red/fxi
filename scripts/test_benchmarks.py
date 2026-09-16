"""Regression checks for benchmark execution, independent of index timings."""
import json
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
                              '    out.write(json.dumps(os.environ["FXI_INDEXES"]) + "\\n")\n')
            binary.chmod(0o755)
            before, after = base / 'before', base / 'after'
            command = [sys.executable, str(Path(__file__).with_name('compare-startup.py')),
                       '--corpus', str(corpus), '--indexes', str(before),
                       '--candidate-indexes', str(after), '--baseline', str(binary),
                       '--candidate', str(binary), '--repetitions', '3',
                       '--patterns', 'definitelyAbsentNeedle', '--output', str(base / 'result.json')]
            subprocess.run(command, env={**os.environ, 'BENCH_TEST_LOG': str(log)},
                           check=True, capture_output=True, timeout=30)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(calls.count(str(before.resolve())), 4)
            self.assertEqual(calls.count(str(after.resolve())), 4)


if __name__ == '__main__':
    unittest.main()
