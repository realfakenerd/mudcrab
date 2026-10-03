import hashlib
import json
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "capture-riverwood-lod.sh"
PROVENANCE_FILES = (
    "bin/engine",
    "assets/skyrim_world.db",
    "assets/cell_cache.rkyv",
    "assets/lod-manifest.json",
    "assets/conversion-manifest.json",
    "assets/integration-report.json",
)
POSTFLIGHT_COUNTERS = (
    "pending_lod_chunks",
    "failed_lod_chunks",
    "pending_lod_queries",
    "failed_lod_queries",
    "failed_cells",
    "loading_cells",
    "pending_asset_instances",
    "pending_surface_instances",
    "asset_load_failures",
    "material_validation_failures",
    "streaming_invariant_failures",
    "diagnostic_fallbacks",
)


def extract_python_block(opener):
    source = SCRIPT.read_text(encoding="utf-8")
    start = source.index(opener)
    body_start = source.index("\n", start) + 1
    body_end = source.index("\nPY\n", body_start)
    return source[body_start:body_end]


PROVENANCE_VALIDATOR = extract_python_block('python3 - "$package" <<\'PY\'')
POSTFLIGHT_VALIDATOR = extract_python_block('python3 - "$output" <<\'PY\'')


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def refresh_checksum(package, relative):
    path = package / "build-provenance.json"
    build = json.loads(path.read_text(encoding="utf-8"))
    build["checksums"][relative] = sha256(package / relative)
    write_json(path, build)


def make_package(package):
    identity = "a" * 64
    (package / "bin").mkdir(parents=True)
    (package / "assets").mkdir()
    (package / "bin/engine").write_bytes(b"engine-binary-fixture")
    (package / "assets/cell_cache.rkyv").write_bytes(b"cell-cache-fixture")
    write_json(
        package / "assets/lod-manifest.json",
        {
            "converter_schema": 17,
            "world_database_schema": 5,
            "build_identity": identity,
            "chunks": 1,
        },
    )
    write_json(
        package / "assets/conversion-manifest.json",
        {"schema_version": 17, "complete": True},
    )
    write_json(
        package / "assets/integration-report.json",
        {"schema_version": 5, "passed": True},
    )
    with sqlite3.connect(package / "assets/skyrim_world.db") as database:
        database.execute("CREATE TABLE schema_info(version INTEGER NOT NULL)")
        database.execute("INSERT INTO schema_info(version) VALUES (5)")
        database.execute(
            "CREATE TABLE lod_build(id INTEGER PRIMARY KEY, build_identity TEXT NOT NULL)"
        )
        database.execute(
            "INSERT INTO lod_build(id, build_identity) VALUES (1, ?)", (identity,)
        )
    build = {
        "format_version": 1,
        "commit": "b" * 40,
        "dirty_worktree": False,
        "lod_build_identity": identity,
        "checksums": {
            relative: sha256(package / relative) for relative in PROVENANCE_FILES
        },
    }
    write_json(package / "build-provenance.json", build)
    return package


def run_provenance_validator(package):
    return subprocess.run(
        [sys.executable, "-c", PROVENANCE_VALIDATOR, str(package)],
        capture_output=True,
        text=True,
        check=False,
    )


def make_postflight_fixture(root, missing_benchmark=None):
    for name in ("riverwood-radius2", "riverwood-radius0"):
        (root / f"{name}-exit-code.txt").write_text("0\n", encoding="utf-8")
        (root / f"{name}.png").write_bytes(b"screenshot-fixture")
        if name != missing_benchmark:
            write_json(
                root / f"{name}-benchmark.json",
                {"passed": True, "scenario": name},
            )
        profile = root / f"{name}-profile/streaming.json"
        write_json(
            profile,
            {
                "aggregate": {
                    **{key: 0 for key in POSTFLIGHT_COUNTERS},
                    "lod_chunks_ready": 1,
                    "visible_lod_terrain_patches": 1,
                }
            },
        )


def run_postflight_validator(root):
    return subprocess.run(
        [sys.executable, "-c", POSTFLIGHT_VALIDATOR, str(root)],
        capture_output=True,
        text=True,
        check=False,
    )


class CaptureRiverwoodGateTests(unittest.TestCase):
    def test_capture_scripts_redirect_logs_without_unsupported_engine_flag(self):
        for script in (SCRIPT, SCRIPT.with_name("capture-lod-phase1.sh")):
            with self.subTest(script=script.name):
                source = script.read_text(encoding="utf-8")
                self.assertNotIn("--log-file", source)
                self.assertIn('2>&1', source)
                self.assertIn('$name.log', source)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_valid_provenance_passes(self):
        package = make_package(self.root / "package")
        result = run_provenance_validator(package)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["b" * 40, "false"])

    def test_provenance_requires_format_commit_boolean_and_checksum_map(self):
        invalid_cases = (
            ("format version", lambda build: build.update(format_version=2), "Unsupported build provenance"),
            ("commit", lambda build: build.update(commit="not-a-commit"), "Invalid build commit"),
            ("boolean type", lambda build: build.update(dirty_worktree=1), "Invalid dirty_worktree flag"),
            ("checksum map", lambda build: build.update(checksums=[]), "Missing build checksums"),
            (
                "required checksum",
                lambda build: build["checksums"].pop("bin/engine"),
                "Invalid build checksum: bin/engine",
            ),
        )
        for label, mutate, expected in invalid_cases:
            with self.subTest(label=label):
                package = make_package(self.root / label.replace(" ", "-"))
                path = package / "build-provenance.json"
                build = json.loads(path.read_text(encoding="utf-8"))
                mutate(build)
                write_json(path, build)
                result = run_provenance_validator(package)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)

    def test_swapped_engine_binary_fails_checksum_validation(self):
        package = make_package(self.root / "package")
        (package / "bin/engine").write_bytes(b"different-engine-binary")
        result = run_provenance_validator(package)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Build checksum mismatch: bin/engine", result.stderr)

    def test_metadata_schema_and_integration_gates(self):
        cases = (
            (
                "converter schema",
                "assets/lod-manifest.json",
                lambda value: value.update(converter_schema=15),
                "Unsupported LOD schemas",
            ),
            (
                "database schema",
                "assets/lod-manifest.json",
                lambda value: value.update(world_database_schema=4),
                "Unsupported LOD schemas",
            ),
            (
                "integration schema",
                "assets/integration-report.json",
                lambda value: value.update(schema_version=4),
                "Asset integration did not pass",
            ),
            (
                "integration result type",
                "assets/integration-report.json",
                lambda value: value.update(passed=1),
                "Asset integration did not pass",
            ),
        )
        for label, relative, mutate, expected in cases:
            with self.subTest(label=label):
                package = make_package(self.root / label.replace(" ", "-"))
                path = package / relative
                metadata = json.loads(path.read_text(encoding="utf-8"))
                mutate(metadata)
                write_json(path, metadata)
                refresh_checksum(package, relative)
                result = run_provenance_validator(package)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)

    def test_database_identity_mismatch_fails(self):
        package = make_package(self.root / "package")
        with sqlite3.connect(package / "assets/skyrim_world.db") as database:
            database.execute(
                "UPDATE lod_build SET build_identity = ? WHERE id = 1", ("c" * 64,)
            )
        refresh_checksum(package, "assets/skyrim_world.db")
        result = run_provenance_validator(package)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Database and manifest LOD identities differ", result.stderr)

    def test_database_schema_mismatch_fails(self):
        package = make_package(self.root / "package")
        with sqlite3.connect(package / "assets/skyrim_world.db") as database:
            database.execute("UPDATE schema_info SET version = 4")
        refresh_checksum(package, "assets/skyrim_world.db")
        result = run_provenance_validator(package)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Database and manifest LOD identities differ", result.stderr)

    def test_manifest_identity_mismatch_fails(self):
        package = make_package(self.root / "package")
        relative = "assets/lod-manifest.json"
        path = package / relative
        manifest = json.loads(path.read_text(encoding="utf-8"))
        manifest["build_identity"] = "c" * 64
        write_json(path, manifest)
        refresh_checksum(package, relative)
        result = run_provenance_validator(package)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Database and manifest LOD identities differ", result.stderr)

    def test_nested_output_is_rejected_before_writes(self):
        package = self.root / "package"
        package.mkdir()
        output = package / "new-capture"
        result = subprocess.run(
            ["bash", str(SCRIPT), str(package), str(output)],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("Capture output must be outside the package", result.stderr)
        self.assertFalse(output.exists())
        self.assertEqual(list(package.iterdir()), [])

    def test_valid_postflight_reports_pass(self):
        output = self.root / "capture"
        output.mkdir()
        make_postflight_fixture(output)
        result = run_postflight_validator(output)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads((output / "capture-report.json").read_text())
        self.assertIs(report["passed"], True)
        self.assertEqual(report["errors"], [])

    def test_missing_benchmark_fails_despite_zero_exit_and_ready_profile(self):
        output = self.root / "capture"
        output.mkdir()
        make_postflight_fixture(output, missing_benchmark="riverwood-radius0")
        result = run_postflight_validator(output)
        self.assertNotEqual(result.returncode, 0)
        report = json.loads((output / "capture-report.json").read_text())
        self.assertIn(
            "riverwood-radius0: missing or invalid benchmark report", report["errors"]
        )
        self.assertEqual(
            (output / "riverwood-radius0-exit-code.txt").read_text().strip(), "0"
        )

    def test_postflight_rejects_nonpassing_or_mislabeled_benchmark(self):
        cases = (
            ("not-passed", {"passed": False}),
            ("wrong-scenario", {"scenario": "other-run"}),
        )
        for label, changes in cases:
            with self.subTest(label=label):
                output = self.root / label
                output.mkdir()
                make_postflight_fixture(output)
                benchmark = output / "riverwood-radius2-benchmark.json"
                report = json.loads(benchmark.read_text(encoding="utf-8"))
                report.update(changes)
                write_json(benchmark, report)
                result = run_postflight_validator(output)
                self.assertNotEqual(result.returncode, 0)
                capture_report = json.loads(
                    (output / "capture-report.json").read_text(encoding="utf-8")
                )
                self.assertIn(
                    "riverwood-radius2: benchmark acceptance did not pass",
                    capture_report["errors"],
                )

    def test_postflight_rejects_every_nonzero_failure_or_pending_counter(self):
        output = self.root / "capture"
        output.mkdir()
        make_postflight_fixture(output)
        profile = output / "riverwood-radius2-profile/streaming.json"
        data = json.loads(profile.read_text(encoding="utf-8"))
        for key in POSTFLIGHT_COUNTERS:
            data["aggregate"][key] = 1
        write_json(profile, data)
        result = run_postflight_validator(output)
        self.assertNotEqual(result.returncode, 0)
        report = json.loads((output / "capture-report.json").read_text())
        for key in POSTFLIGHT_COUNTERS:
            self.assertIn(f"riverwood-radius2: {key}=1", report["errors"])

    def test_postflight_rejects_missing_counter(self):
        output = self.root / "capture"
        output.mkdir()
        make_postflight_fixture(output)
        profile = output / "riverwood-radius0-profile/streaming.json"
        data = json.loads(profile.read_text(encoding="utf-8"))
        del data["aggregate"]["pending_lod_queries"]
        write_json(profile, data)
        result = run_postflight_validator(output)
        self.assertNotEqual(result.returncode, 0)
        report = json.loads((output / "capture-report.json").read_text())
        self.assertIn("riverwood-radius0: pending_lod_queries=None", report["errors"])


if __name__ == "__main__":
    unittest.main()
