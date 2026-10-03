import contextlib
import hashlib
import importlib.util
import io
import json
import struct
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "audit-riverwood-reuse.py"
SPEC = importlib.util.spec_from_file_location("audit_riverwood_reuse", SCRIPT)
AUDIT = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(AUDIT)


def make_glb(document, tail=b""):
    json_bytes = json.dumps(document, separators=(",", ":"), allow_nan=False).encode("utf-8")
    json_bytes += b" " * ((-len(json_bytes)) % 4)
    chunk = struct.pack("<II", len(json_bytes), 0x4E4F534A) + json_bytes
    if tail:
        chunk += tail
    return struct.pack("<4sII", b"glTF", 2, 12 + len(chunk)) + chunk


def base_document():
    return {
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"name": "Scene"}],
        "materials": [{"name": "stone", "pbrMetallicRoughness": {"roughnessFactor": 0.8}}],
    }


class AuditRiverwoodReuseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.candidate = self.root / "candidate"
        self.reference = self.root / "reference"
        for root in (self.candidate, self.reference):
            for directory in ("meshes", "textures", "scripts"):
                (root / directory).mkdir(parents=True, exist_ok=True)
        self.manifest = self.reference / "conversion-manifest.json"

    def tearDown(self):
        self.temp.cleanup()

    def write_asset(self, root, relative, data):
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def write_manifest(self, outputs):
        entries = {}
        for index, (relative, data) in enumerate(outputs.items()):
            entries[str(index)] = {
                "output": relative,
                "output_hash": hashlib.sha256(data).hexdigest(),
                "output_size": len(data),
                "source_hash": "0" * 64,
            }
        self.manifest.write_text(
            json.dumps({"complete": True, "schema_version": 15, "entries": entries}),
            encoding="utf-8",
        )

    def write_pair(self, relative, reference_bytes, candidate_bytes=None, manifest_path=None):
        self.write_asset(self.reference, relative, reference_bytes)
        self.write_asset(
            self.candidate,
            relative,
            reference_bytes if candidate_bytes is None else candidate_bytes,
        )
        self.write_manifest({manifest_path or relative: reference_bytes})

    def prepare_inventories(self):
        candidate_document = AUDIT.inventory(self.candidate)
        candidate_path = self.root / "candidate-inventory.json"
        candidate_path.write_text(
            json.dumps(candidate_document, sort_keys=True, separators=(",", ":")),
            encoding="utf-8",
        )
        reference_document = AUDIT.reference_inventory(
            self.reference, candidate_path, self.manifest
        )
        reference_path = self.root / "reference-inventory.json"
        reference_path.write_text(
            json.dumps(reference_document, sort_keys=True, separators=(",", ":")),
            encoding="utf-8",
        )
        return candidate_path, reference_path, candidate_document, reference_document

    def compare(self):
        candidate_path, reference_path, _, _ = self.prepare_inventories()
        return AUDIT.compare(candidate_path, reference_path)

    def test_collision_addition_and_new_empty_extras_are_permitted(self):
        reference = make_glb(base_document())
        candidate_document = base_document()
        candidate_document["scenes"][0]["extras"] = {
            "openSkyrimCollision": {"version": 1, "shapes": [{"kind": "box"}]}
        }
        self.write_pair("meshes/marker.glb", reference, make_glb(candidate_document))
        result = self.compare()
        self.assertTrue(result["passed"], result["errors"])
        candidate_identity = result["files"][0]["candidate_normalized"]
        reference_identity = result["files"][0]["reference_normalized"]
        self.assertTrue(candidate_identity["collision_annotation_present"])
        self.assertFalse(reference_identity["collision_annotation_present"])
        self.assertEqual(
            candidate_identity["normalized_json_sha256"],
            reference_identity["normalized_json_sha256"],
        )

    def test_material_change_is_rejected(self):
        reference_document = base_document()
        candidate_document = base_document()
        candidate_document["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"] = 0.4
        self.write_pair(
            "meshes/marker.glb",
            make_glb(reference_document),
            make_glb(candidate_document),
        )
        result = self.compare()
        self.assertFalse(result["passed"])
        self.assertIn("normalized structured GLB JSON differs", result["files"][0]["issues"])

    def test_other_scene_change_is_rejected(self):
        reference_document = base_document()
        reference_document["scenes"].append({"name": "Other"})
        candidate_document = json.loads(json.dumps(reference_document))
        candidate_document["scenes"][1]["name"] = "Changed"
        self.write_pair(
            "meshes/marker.glb",
            make_glb(reference_document),
            make_glb(candidate_document),
        )
        self.assertFalse(self.compare()["passed"])

    def test_scene_zero_extras_must_be_an_object(self):
        document = base_document()
        document["scenes"][0]["extras"] = None
        self.write_asset(self.candidate, "meshes/bad.glb", make_glb(document))
        with self.assertRaises(AUDIT.AuditError):
            AUDIT.inventory(self.candidate)

    def test_bin_chunk_change_is_rejected(self):
        document = base_document()
        json_data = json.dumps(document, separators=(",", ":")).encode()
        json_data += b" " * ((-len(json_data)) % 4)

        def with_bin(bin_data):
            json_chunk = struct.pack("<II", len(json_data), 0x4E4F534A) + json_data
            bin_chunk = struct.pack("<II", len(bin_data), 0x004E4942) + bin_data
            body = json_chunk + bin_chunk
            return struct.pack("<4sII", b"glTF", 2, 12 + len(body)) + body

        self.write_pair("meshes/model.glb", with_bin(b"abcd"), with_bin(b"abce"))
        result = self.compare()
        self.assertFalse(result["passed"])
        self.assertIn("bytes or chunks after the GLB JSON chunk differ", result["files"][0]["issues"])

    def test_existing_collision_annotation_cannot_be_removed(self):
        reference_document = base_document()
        reference_document["scenes"][0]["extras"] = {"openSkyrimCollision": {"version": 1}}
        self.write_pair(
            "meshes/model.glb",
            make_glb(reference_document),
            make_glb(base_document()),
        )
        result = self.compare()
        self.assertFalse(result["passed"])
        self.assertTrue(any("removed a reference collision" in issue for issue in result["errors"]))

    def test_existing_collision_annotation_cannot_be_changed(self):
        reference_document = base_document()
        reference_document["scenes"][0]["extras"] = {"openSkyrimCollision": {"version": 1}}
        candidate_document = base_document()
        candidate_document["scenes"][0]["extras"] = {"openSkyrimCollision": {"version": 2}}
        self.write_pair(
            "meshes/model.glb",
            make_glb(reference_document),
            make_glb(candidate_document),
        )
        result = self.compare()
        self.assertFalse(result["passed"])
        self.assertTrue(any("changed an existing reference collision" in issue for issue in result["errors"]))

    def test_malformed_glb_and_duplicate_or_nonfinite_json_fail_closed(self):
        malformed = (
            b"glTF" + struct.pack("<II", 2, 20) + struct.pack("<II", 0, 0x4E4F534A),
            make_glb(base_document())[:-1],
            struct.pack("<4sII", b"glTF", 2, 24)
            + struct.pack("<II", 64, 0x4E4F534A)
            + b"{}  ",
            make_glb(base_document()).replace(b'"scene":0', b'"scene":NaN'),
            make_glb(base_document()).replace(b'"scene":0', b'"scene":0,"scene":0'),
        )
        for raw in malformed:
            with self.subTest(raw=raw[:24]):
                self.write_asset(self.candidate, "meshes/bad.glb", raw)
                with self.assertRaises(AUDIT.AuditError):
                    AUDIT.inventory(self.candidate)
                (self.candidate / "meshes/bad.glb").unlink()

    def test_malicious_inventory_and_manifest_paths_fail_closed(self):
        bad_inventory = {
            "format": AUDIT.INVENTORY_FORMAT,
            "files": [
                {
                    "path": "../../outside.glb",
                    "kind": "glb",
                    "size": 0,
                    "sha256": "0" * 64,
                    "glb": {},
                }
            ],
        }
        path = self.root / "bad-inventory.json"
        path.write_text(json.dumps(bad_inventory), encoding="utf-8")
        with self.assertRaises(AUDIT.AuditError):
            AUDIT._load_inventory(path)

        bad_manifest = self.root / "bad-manifest.json"
        bad_manifest.write_text(
            json.dumps(
                {
                    "complete": True,
                    "entries": {
                        "escape": {
                            "output": "../../outside.ktx2",
                            "output_hash": "0" * 64,
                            "output_size": 0,
                        }
                    },
                }
            ),
            encoding="utf-8",
        )
        with self.assertRaises(AUDIT.AuditError):
            AUDIT._load_manifest(bad_manifest)

    def test_candidate_and_selected_reference_symlinks_fail_closed(self):
        outside = self.root / "outside.ktx2"
        outside.write_bytes(b"texture")
        candidate_link = self.candidate / "textures" / "linked.ktx2"
        candidate_link.symlink_to(outside)
        with self.assertRaises(AUDIT.AuditError):
            AUDIT.inventory(self.candidate)
        candidate_link.unlink()

        self.write_asset(self.candidate, "textures/stone.ktx2", b"texture")
        reference_link = self.reference / "textures" / "stone.ktx2"
        reference_link.symlink_to(outside)
        self.write_manifest({"textures/stone.ktx2": b"texture"})
        candidate_path = self.root / "candidate-inventory.json"
        candidate_path.write_text(json.dumps(AUDIT.inventory(self.candidate)), encoding="utf-8")
        with self.assertRaises(AUDIT.AuditError):
            AUDIT.reference_inventory(self.reference, candidate_path, self.manifest)

    def test_reference_manifest_hash_and_size_are_enforced(self):
        payload = b"\xAB\xCDtexture"
        self.write_pair("textures/stone.ktx2", payload)
        candidate_path = self.root / "candidate-inventory.json"
        candidate_path.write_text(json.dumps(AUDIT.inventory(self.candidate)), encoding="utf-8")
        for field, value in (
            ("output_hash", "0" * 64),
            ("output_size", len(payload) + 1),
        ):
            with self.subTest(field=field):
                manifest = json.loads(self.manifest.read_text(encoding="utf-8"))
                manifest["entries"]["0"][field] = value
                self.manifest.write_text(json.dumps(manifest), encoding="utf-8")
                with self.assertRaises(AUDIT.AuditError):
                    AUDIT.reference_inventory(self.reference, candidate_path, self.manifest)
                self.write_manifest({"textures/stone.ktx2": payload})

    def test_non_glb_assets_require_exact_bytes(self):
        self.write_pair("textures/stone.ktx2", b"\xAB\xCDtexture", b"\xAB\xCDtexTure")
        result = self.compare()
        self.assertFalse(result["passed"])
        self.assertIn("non-GLB bytes differ", result["files"][0]["issues"])

    def test_srgb_alias_uses_the_base_ktx2_manifest_provenance(self):
        payload = b"identical texture bytes"
        self.write_asset(self.reference, "textures/stone.ktx2", payload)
        self.write_asset(self.reference, "textures/stone.opensky-srgb.ktx2", payload)
        self.write_asset(self.candidate, "textures/stone.ktx2", payload)
        self.write_asset(self.candidate, "textures/stone.opensky-srgb.ktx2", payload)
        self.write_manifest({"textures/stone.ktx2": payload})
        result = self.compare()
        self.assertTrue(result["passed"], result["errors"])
        alias_result = next(
            item
            for item in result["files"]
            if item["path"].endswith(".opensky-srgb.ktx2")
        )
        self.assertEqual(alias_result["reference_provenance"]["output"], "textures/stone.ktx2")

    def test_runtime_uses_explicit_baseline_self_supplemental_provenance(self):
        runtime = b"local runtime = true\n"
        self.write_asset(self.reference, "scripts/papyrus_runtime.luau", runtime)
        self.write_asset(self.candidate, "scripts/papyrus_runtime.luau", runtime)
        self.write_manifest({})
        result = self.compare()
        self.assertTrue(result["passed"], result["errors"])
        self.assertEqual(
            result["files"][0]["reference_provenance"]["kind"],
            "baseline-self-supplemental",
        )

    def test_reference_inventory_only_reads_candidate_subset(self):
        selected = b"selected texture"
        uninstalled = b"other texture"
        self.write_asset(self.reference, "textures/selected.ktx2", selected)
        self.write_asset(self.candidate, "textures/selected.ktx2", selected)
        self.write_asset(self.reference, "textures/uninstalled.ktx2", uninstalled)
        self.write_asset(self.reference, "meshes/uninstalled.glb", b"not a GLB")
        (self.root / "outside.ktx2").write_bytes(b"outside")
        (self.reference / "textures" / "unselected-link.ktx2").symlink_to(
            self.root / "outside.ktx2"
        )
        self.write_manifest(
            {
                "textures/selected.ktx2": selected,
                "textures/uninstalled.ktx2": uninstalled,
                "meshes/uninstalled.glb": b"not a GLB",
                "textures/not-installed.ktx2": b"absent but manifested",
            }
        )
        candidate_path, reference_path, _, reference_document = self.prepare_inventories()
        self.assertEqual(len(reference_document["files"]), 1)
        self.assertTrue(AUDIT.compare(candidate_path, reference_path)["passed"])

    def test_compare_is_portable_and_needs_only_inventory_files(self):
        self.write_pair("textures/stone.ktx2", b"texture")
        candidate_path, reference_path, _, _ = self.prepare_inventories()
        self.candidate.rename(self.root / "candidate-moved")
        self.reference.rename(self.root / "reference-moved")
        result = AUDIT.compare(candidate_path, reference_path)
        self.assertTrue(result["passed"], result["errors"])

    def test_path_set_mismatch_fails(self):
        self.write_pair("textures/stone.ktx2", b"texture")
        candidate_path, reference_path, _, reference_document = self.prepare_inventories()
        reference_document["files"] = []
        reference_path.write_text(json.dumps(reference_document), encoding="utf-8")
        result = AUDIT.compare(candidate_path, reference_path)
        self.assertFalse(result["passed"])
        self.assertEqual(result["path_set"]["candidate_only"], ["textures/stone.ktx2"])

    def test_reference_only_path_mismatch_fails(self):
        stone = b"stone"
        extra = b"extra"
        self.write_asset(self.candidate, "textures/stone.ktx2", stone)
        self.write_asset(self.reference, "textures/stone.ktx2", stone)
        self.write_asset(self.reference, "textures/extra.ktx2", extra)
        self.write_manifest(
            {"textures/stone.ktx2": stone, "textures/extra.ktx2": extra}
        )
        candidate_path, reference_path, _, reference_document = self.prepare_inventories()
        extra_hash = hashlib.sha256(extra).hexdigest()
        reference_document["files"].append(
            {
                "path": "textures/extra.ktx2",
                "kind": "ktx2",
                "size": len(extra),
                "sha256": extra_hash,
                "provenance": {
                    "kind": "conversion-manifest",
                    "output": "textures/extra.ktx2",
                    "output_sha256": extra_hash,
                    "output_size": len(extra),
                },
            }
        )
        reference_document["files"].sort(key=lambda item: item["path"])
        reference_path.write_text(json.dumps(reference_document), encoding="utf-8")
        result = AUDIT.compare(candidate_path, reference_path)
        self.assertFalse(result["passed"])
        self.assertEqual(result["path_set"]["reference_only"], ["textures/extra.ktx2"])

    def test_reference_manifest_duplicate_outputs_fail_closed(self):
        payload = b"texture"
        self.write_asset(self.reference, "textures/a.ktx2", payload)
        self.write_asset(self.candidate, "textures/a.ktx2", payload)
        digest = hashlib.sha256(payload).hexdigest()
        self.manifest.write_text(
            json.dumps(
                {
                    "complete": True,
                    "entries": {
                        "one": {
                            "output": "textures/a.ktx2",
                            "output_hash": digest,
                            "output_size": len(payload),
                        },
                        "two": {
                            "output": "textures/a.ktx2",
                            "output_hash": digest,
                            "output_size": len(payload),
                        },
                    },
                }
            ),
            encoding="utf-8",
        )
        candidate_path = self.root / "candidate-inventory.json"
        candidate_path.write_text(json.dumps(AUDIT.inventory(self.candidate)), encoding="utf-8")
        with self.assertRaises(AUDIT.AuditError):
            AUDIT.reference_inventory(self.reference, candidate_path, self.manifest)

    def test_cli_returns_nonzero_for_inventory_mismatch(self):
        self.write_pair("textures/stone.ktx2", b"baseline", b"candidate")
        candidate_path, reference_path, _, _ = self.prepare_inventories()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = AUDIT.main(
                [
                    "compare",
                    "--candidate-inventory",
                    str(candidate_path),
                    "--reference-inventory",
                    str(reference_path),
                ]
            )
        self.assertEqual(status, 1)
        self.assertFalse(json.loads(output.getvalue())["passed"])

    def test_cli_inventory_and_reference_inventory_emit_json(self):
        self.write_pair("textures/stone.ktx2", b"texture")
        candidate_path = self.root / "candidate-inventory.json"
        candidate_output = io.StringIO()
        with contextlib.redirect_stdout(candidate_output):
            status = AUDIT.main(["inventory", str(self.candidate)])
        self.assertEqual(status, 0)
        candidate_path.write_text(candidate_output.getvalue(), encoding="utf-8")

        reference_output = io.StringIO()
        with contextlib.redirect_stdout(reference_output):
            status = AUDIT.main(
                [
                    "reference-inventory",
                    str(self.reference),
                    "--candidate-inventory",
                    str(candidate_path),
                    "--manifest",
                    str(self.manifest),
                ]
            )
        self.assertEqual(status, 0)
        self.assertEqual(
            json.loads(reference_output.getvalue())["format"],
            AUDIT.REFERENCE_FORMAT,
        )


if __name__ == "__main__":
    unittest.main()
