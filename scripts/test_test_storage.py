"""Subprocess regressions for the real app-data pollution guard."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check-test-storage.py")


class TestStorageGuardTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / "app-data"
        self.state = self.base / "before.json"

    def run_guard(self, operation):
        return subprocess.run([sys.executable, str(SCRIPT), operation, str(self.state),
                               "--path", str(self.root)], capture_output=True, text=True,
                              timeout=10)

    def record(self):
        result = self.run_guard("snapshot")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_unchanged_tree_passes_and_same_length_write_is_detected(self):
        self.root.mkdir()
        file = self.root / "index"
        file.write_text("before", encoding="utf-8")
        original = file.stat()
        self.record()
        self.assertEqual(self.run_guard("verify").returncode, 0)
        file.write_text("after!", encoding="utf-8")
        os.utime(file, ns=(original.st_atime_ns, original.st_mtime_ns + 2_000_000_000))
        result = self.run_guard("verify")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("index", result.stderr)

    def test_directory_and_root_metadata_changes_are_detected(self):
        self.root.mkdir()
        directory = self.root / "empty"
        directory.mkdir()
        for target in [directory, self.root]:
            self.record()
            original = target.stat()
            os.utime(target, ns=(original.st_atime_ns, original.st_mtime_ns + 2_000_000_000))
            self.assertEqual(self.run_guard("verify").returncode, 1)

    def test_missing_path_is_distinct_from_an_empty_directory(self):
        self.record()
        self.assertFalse(self.root.exists(), "snapshot must not create real app data")
        self.assertEqual(self.run_guard("verify").returncode, 0)
        self.root.mkdir()
        self.assertEqual(self.run_guard("verify").returncode, 1)
        self.record()
        self.root.rmdir()
        self.assertEqual(self.run_guard("verify").returncode, 1)

    def test_added_and_removed_files_are_detected(self):
        self.root.mkdir()
        self.record()
        file = self.root / "new-index"
        file.write_bytes(b"x")
        self.assertEqual(self.run_guard("verify").returncode, 1)
        self.record()
        file.unlink()
        self.assertEqual(self.run_guard("verify").returncode, 1)

    def test_symlink_targets_are_not_traversed(self):
        self.root.mkdir()
        outside = self.base / "outside"
        outside.mkdir()
        (outside / "data").write_text("before", encoding="utf-8")
        try:
            (self.root / "link").symlink_to(outside, target_is_directory=True)
        except OSError as error:
            self.skipTest(f"symlink creation unavailable: {error}")
        self.record()
        saved = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertEqual(set(saved["snapshot"]["entries"]), {".", "link"})
        (outside / "data").write_text("changed outside", encoding="utf-8")
        self.assertEqual(self.run_guard("verify").returncode, 0)


if __name__ == "__main__":
    unittest.main()
