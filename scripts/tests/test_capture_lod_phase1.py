import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "capture-lod-phase1.sh"
SOURCE = SCRIPT.read_text(encoding="utf-8")
START = SOURCE.index('python3 - "$output/terrain-lod-profile/streaming.json"')
BODY_START = SOURCE.index("\n", START) + 1
VALIDATOR = SOURCE[BODY_START:SOURCE.index("\nPY\n", BODY_START)]


class Phase1CaptureTests(unittest.TestCase):
    def validate(self, metrics):
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory) / "profile.json"
            profile.write_text(json.dumps({"aggregate": metrics}), encoding="utf-8")
            return subprocess.run(
                [sys.executable, "-c", VALIDATOR, str(profile)],
                capture_output=True, text=True, check=False,
            )

    def test_fault_free_query_acceptance(self):
        clean = {
            "lod_chunks_ready": 1,
            "visible_lod_terrain_patches": 1,
            "failed_lod_chunks": 0,
            "pending_lod_chunks": 0,
            "failed_lod_queries": 0,
            "pending_lod_queries": 0,
        }
        self.assertEqual(self.validate(clean).returncode, 0)
        for key in ("failed_lod_queries", "pending_lod_queries"):
            with self.subTest(counter=key):
                self.assertNotEqual(self.validate({**clean, key: 1}).returncode, 0)
                missing = clean.copy()
                del missing[key]
                self.assertNotEqual(self.validate(missing).returncode, 0)

    def test_worktree_local_target_default(self):
        self.assertIn("CARGO_TARGET_DIR:-$repo/target", SOURCE)
