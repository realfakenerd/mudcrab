#!/usr/bin/env python3
"""Read-only structural reuse check for installed OpenSkyrim assets."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import math
import os
import re
import stat
import struct
import sys
from pathlib import Path, PurePosixPath
from typing import Any


INVENTORY_FORMAT = "openskyrim-riverwood-reuse-inventory-v1"
REFERENCE_FORMAT = "openskyrim-riverwood-reuse-reference-v1"
COMPARISON_FORMAT = "openskyrim-riverwood-reuse-comparison-v1"
ASSET_ROOTS = {
    "meshes": ".glb",
    "textures": ".ktx2",
    "scripts": ".luau",
}
ALIAS_SUFFIX = ".opensky-srgb.ktx2"
RUNTIME_PATH = "scripts/papyrus_runtime.luau"
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
JSON_CHUNK_TYPE = 0x4E4F534A
MISSING = object()


class AuditError(Exception):
    pass


def _reject_constant(value: str) -> None:
    raise AuditError(f"nonfinite JSON number {value!r}")


def _finite_float(value: str) -> float:
    number = float(value)
    if not math.isfinite(number):
        raise AuditError(f"nonfinite JSON number {value!r}")
    return number


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise AuditError(f"duplicate JSON object key {key!r}")
        result[key] = value
    return result


def _load_json(data: bytes, description: str) -> Any:
    try:
        text = data.decode("utf-8")
        return json.loads(
            text,
            object_pairs_hook=_unique_object,
            parse_constant=_reject_constant,
            parse_float=_finite_float,
        )
    except AuditError as exc:
        raise AuditError(f"{description}: {exc}") from exc
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as exc:
        raise AuditError(f"{description}: invalid UTF-8 JSON: {exc}") from exc


def _canonical_json(value: Any) -> bytes:
    try:
        return json.dumps(
            value,
            ensure_ascii=False,
            allow_nan=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
    except (TypeError, ValueError, UnicodeEncodeError) as exc:
        raise AuditError(f"cannot canonicalize JSON: {exc}") from exc


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _hash_file(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    try:
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(path, flags)
        with os.fdopen(descriptor, "rb") as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise AuditError(f"not a regular file: {path}")
            while block := stream.read(1024 * 1024):
                digest.update(block)
                size += len(block)
    except AuditError:
        raise
    except OSError as exc:
        raise AuditError(f"cannot read {path}: {exc}") from exc
    return digest.hexdigest(), size


def _read_file(path: Path) -> bytes:
    try:
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(path, flags)
        with os.fdopen(descriptor, "rb") as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise AuditError(f"not a regular file: {path}")
            return stream.read()
    except AuditError:
        raise
    except OSError as exc:
        raise AuditError(f"cannot read {path}: {exc}") from exc


def _validate_relative_path(value: Any, description: str) -> str:
    if not isinstance(value, str) or not value:
        raise AuditError(f"{description}: path must be a nonempty string")
    if "\\" in value or "\x00" in value or value.startswith("/") or "//" in value:
        raise AuditError(f"{description}: unsafe path {value!r}")
    parts = value.split("/")
    if any(part in ("", ".", "..") for part in parts):
        raise AuditError(f"{description}: unsafe path {value!r}")
    try:
        value.encode("utf-8")
    except UnicodeEncodeError as exc:
        raise AuditError(f"{description}: path is not valid UTF-8") from exc
    pure = PurePosixPath(value)
    if pure.is_absolute() or pure.as_posix() != value:
        raise AuditError(f"{description}: noncanonical path {value!r}")
    suffix = ASSET_ROOTS.get(parts[0])
    if suffix is None or len(parts) < 2 or not value.endswith(suffix):
        raise AuditError(f"{description}: unsupported installed asset path {value!r}")
    if suffix == ".ktx2" and value.endswith(ALIAS_SUFFIX):
        return value
    return value


def _kind_for_path(path: str) -> str:
    suffix = ASSET_ROOTS[PurePosixPath(path).parts[0]]
    return {".glb": "glb", ".ktx2": "ktx2", ".luau": "luau"}[suffix]


def _resolved_root(root: str | Path) -> Path:
    path = Path(root).absolute()
    try:
        resolved = path.resolve(strict=True)
    except OSError as exc:
        raise AuditError(f"asset root does not exist: {path}: {exc}") from exc
    if resolved != path or not path.is_dir():
        raise AuditError(f"asset root must be a real directory without symlink components: {path}")
    return path


def _scan_paths(root: Path) -> dict[str, Path]:
    found: dict[str, Path] = {}

    def visit(directory: Path, relative: PurePosixPath) -> None:
        try:
            entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
        except OSError as exc:
            raise AuditError(f"cannot scan {directory}: {exc}") from exc
        for entry in entries:
            path = Path(entry.path)
            rel = relative / entry.name
            if entry.is_symlink():
                raise AuditError(f"symlink is not allowed in installed asset roots: {path}")
            try:
                if entry.is_dir(follow_symlinks=False):
                    visit(path, rel)
                    continue
                if not entry.is_file(follow_symlinks=False):
                    raise AuditError(f"non-regular filesystem entry in installed assets: {path}")
            except OSError as exc:
                raise AuditError(f"cannot inspect {path}: {exc}") from exc
            relative_path = rel.as_posix()
            if not any(relative_path.endswith(suffix) for suffix in ASSET_ROOTS.values()):
                continue
            relative_path = _validate_relative_path(relative_path, "installed asset")
            if relative_path in found:
                raise AuditError(f"duplicate installed asset path {relative_path!r}")
            found[relative_path] = path

    for directory_name in ASSET_ROOTS:
        directory = root / directory_name
        try:
            mode = directory.lstat().st_mode
        except FileNotFoundError:
            continue
        except OSError as exc:
            raise AuditError(f"cannot inspect asset directory {directory}: {exc}") from exc
        if stat.S_ISLNK(mode):
            raise AuditError(f"symlink is not allowed for installed asset directory: {directory}")
        if not stat.S_ISDIR(mode):
            raise AuditError(f"installed asset root is not a directory: {directory}")
        visit(directory, PurePosixPath(directory_name))
    return dict(sorted(found.items()))


def _inspect_glb(data: bytes, description: str) -> tuple[dict[str, Any], bytes, bytes]:
    if len(data) < 20:
        raise AuditError(f"{description}: truncated GLB header or JSON chunk")
    magic, version, declared_length = struct.unpack_from("<4sII", data, 0)
    if magic != b"glTF" or version != 2:
        raise AuditError(f"{description}: expected a GLB version 2 header")
    if declared_length != len(data):
        raise AuditError(
            f"{description}: declared GLB length {declared_length} differs from actual {len(data)}"
        )

    offset = 12
    chunk_index = 0
    json_bytes: bytes | None = None
    post_json = b""
    while offset < len(data):
        if len(data) - offset < 8:
            raise AuditError(f"{description}: truncated GLB chunk header")
        chunk_length, chunk_type = struct.unpack_from("<II", data, offset)
        if chunk_length % 4 != 0:
            raise AuditError(f"{description}: GLB chunk length is not 4-byte aligned")
        chunk_start = offset + 8
        chunk_end = chunk_start + chunk_length
        if chunk_end > len(data):
            raise AuditError(f"{description}: GLB chunk exceeds declared file length")
        if chunk_index == 0:
            if chunk_type != JSON_CHUNK_TYPE:
                raise AuditError(f"{description}: first GLB chunk is not JSON")
            json_bytes = data[chunk_start:chunk_end]
            post_json = data[chunk_end:]
        elif chunk_type == JSON_CHUNK_TYPE:
            raise AuditError(f"{description}: GLB contains more than one JSON chunk")
        offset = chunk_end
        chunk_index += 1
    if offset != len(data) or json_bytes is None:
        raise AuditError(f"{description}: malformed GLB chunk table")

    document = _load_json(json_bytes, f"{description} GLB JSON chunk")
    if not isinstance(document, dict):
        raise AuditError(f"{description}: GLB JSON root must be an object")
    scenes = document.get("scenes")
    if not isinstance(scenes, list) or not scenes or not isinstance(scenes[0], dict):
        raise AuditError(f"{description}: GLB JSON must contain an object at scenes[0]")
    extras = scenes[0].get("extras", MISSING)
    if extras is not MISSING and not isinstance(extras, dict):
        raise AuditError(f"{description}: scenes[0].extras must be an object")
    has_collision = isinstance(extras, dict) and "openSkyrimCollision" in extras
    collision_hash = None
    normalized = copy.deepcopy(document)
    if has_collision:
        collision_hash = _sha256(_canonical_json(extras["openSkyrimCollision"]))
        normalized_extras = normalized["scenes"][0]["extras"]
        del normalized_extras["openSkyrimCollision"]
        if not normalized_extras:
            del normalized["scenes"][0]["extras"]
    normalized_bytes = _canonical_json(normalized)
    post_hash = _sha256(post_json)
    normalized_hash = _sha256(normalized_bytes)
    render_digest = hashlib.sha256()
    render_digest.update(b"OpenSkyrim-GLB-render-identity-v1\0")
    render_digest.update(bytes.fromhex(normalized_hash))
    render_digest.update(struct.pack(">Q", len(post_json)))
    render_digest.update(bytes.fromhex(post_hash))
    identity = {
        "normalized_json_sha256": normalized_hash,
        "post_json_sha256": post_hash,
        "post_json_size": len(post_json),
        "render_identity_sha256": render_digest.hexdigest(),
        "collision_annotation_present": has_collision,
        "collision_annotation_sha256": collision_hash,
    }
    return identity, normalized_bytes, post_json


def _inventory_entry(path: str, file_path: Path) -> dict[str, Any]:
    kind = _kind_for_path(path)
    if kind == "glb":
        data = _read_file(file_path)
        identity, _, _ = _inspect_glb(data, path)
        return {
            "path": path,
            "kind": kind,
            "size": len(data),
            "sha256": _sha256(data),
            "glb": identity,
        }
    digest, size = _hash_file(file_path)
    return {"path": path, "kind": kind, "size": size, "sha256": digest}


def inventory(root: str | Path) -> dict[str, Any]:
    resolved = _resolved_root(root)
    paths = _scan_paths(resolved)
    files = [_inventory_entry(path, file_path) for path, file_path in paths.items()]
    return {"format": INVENTORY_FORMAT, "files": files}


def _valid_digest(value: Any, description: str) -> str:
    if not isinstance(value, str) or not SHA256_RE.fullmatch(value):
        raise AuditError(f"{description}: expected lowercase SHA-256")
    return value


def _validate_inventory(
    value: Any, *, reference: bool = False
) -> dict[str, dict[str, Any]]:
    if reference:
        expected_document_keys = {
            "format",
            "candidate_inventory_sha256",
            "manifest_sha256",
            "files",
        }
        expected_format = REFERENCE_FORMAT
    else:
        expected_document_keys = {"format", "files"}
        expected_format = INVENTORY_FORMAT
    if not isinstance(value, dict) or set(value) != expected_document_keys:
        label = "reference inventory" if reference else "candidate inventory"
        raise AuditError(f"{label} has unexpected top-level fields")
    if value["format"] != expected_format or not isinstance(value["files"], list):
        raise AuditError("unsupported inventory format")
    if reference:
        _valid_digest(value["candidate_inventory_sha256"], "reference candidate inventory hash")
        _valid_digest(value["manifest_sha256"], "reference manifest hash")
    files: dict[str, dict[str, Any]] = {}
    previous_path: str | None = None
    for item in value["files"]:
        if not isinstance(item, dict):
            raise AuditError("inventory file entries must be objects")
        path = _validate_relative_path(item.get("path"), "candidate inventory")
        kind = _kind_for_path(path)
        expected_keys = {"path", "kind", "size", "sha256"}
        if kind == "glb":
            expected_keys.add("glb")
        if reference:
            expected_keys.add("provenance")
        if set(item) != expected_keys:
            raise AuditError(f"candidate inventory entry has unexpected fields: {path}")
        if item["kind"] != kind:
            raise AuditError(f"candidate inventory kind does not match path: {path}")
        if type(item["size"]) is not int or item["size"] < 0:
            raise AuditError(f"candidate inventory size is invalid: {path}")
        _valid_digest(item["sha256"], f"candidate inventory {path}")
        if kind == "glb":
            glb = item["glb"]
            if not isinstance(glb, dict) or set(glb) != {
                "normalized_json_sha256",
                "post_json_sha256",
                "post_json_size",
                "render_identity_sha256",
                "collision_annotation_present",
                "collision_annotation_sha256",
            }:
                raise AuditError(f"candidate inventory GLB identity is malformed: {path}")
            normalized_hash = _valid_digest(
                glb["normalized_json_sha256"], f"candidate inventory {path} normalized JSON"
            )
            post_hash = _valid_digest(
                glb["post_json_sha256"], f"candidate inventory {path} post-JSON bytes"
            )
            if type(glb["post_json_size"]) is not int or glb["post_json_size"] < 0:
                raise AuditError(f"candidate inventory post-JSON size is invalid: {path}")
            if type(glb["collision_annotation_present"]) is not bool:
                raise AuditError(f"candidate inventory collision presence is invalid: {path}")
            collision_hash = glb["collision_annotation_sha256"]
            if glb["collision_annotation_present"]:
                _valid_digest(collision_hash, f"candidate inventory {path} collision field")
            elif collision_hash is not None:
                raise AuditError(f"candidate inventory has hash for absent collision field: {path}")
            render_digest = hashlib.sha256()
            render_digest.update(b"OpenSkyrim-GLB-render-identity-v1\0")
            render_digest.update(bytes.fromhex(normalized_hash))
            render_digest.update(struct.pack(">Q", glb["post_json_size"]))
            render_digest.update(bytes.fromhex(post_hash))
            if glb["render_identity_sha256"] != render_digest.hexdigest():
                raise AuditError(f"candidate inventory GLB identity is inconsistent: {path}")
        if reference:
            provenance = item["provenance"]
            if not isinstance(provenance, dict):
                raise AuditError(f"reference provenance is malformed: {path}")
            provenance_kind = provenance.get("kind")
            if provenance_kind == "conversion-manifest":
                if set(provenance) != {
                    "kind",
                    "output",
                    "output_sha256",
                    "output_size",
                }:
                    raise AuditError(f"reference manifest provenance is malformed: {path}")
                output = _validate_relative_path(
                    provenance["output"], f"reference provenance {path}"
                )
                output_hash = _valid_digest(
                    provenance["output_sha256"], f"reference provenance {path} hash"
                )
                output_size = provenance["output_size"]
                if type(output_size) is not int or output_size < 0:
                    raise AuditError(f"reference provenance has invalid size: {path}")
                if (item["sha256"], item["size"]) != (output_hash, output_size):
                    raise AuditError(f"reference fingerprint disagrees with manifest: {path}")
                expected_output = _manifest_source_path(path)
                if output != expected_output:
                    raise AuditError(f"reference provenance path is invalid: {path}")
            elif provenance_kind == "baseline-self-supplemental":
                if (
                    set(provenance) != {"kind", "output"}
                    or path != RUNTIME_PATH
                    or provenance["output"] != path
                ):
                    raise AuditError(f"reference supplemental provenance is invalid: {path}")
            else:
                raise AuditError(f"unknown reference provenance kind: {path}")
        if path in files:
            raise AuditError(f"duplicate candidate inventory path {path!r}")
        if previous_path is not None and path < previous_path:
            raise AuditError("candidate inventory paths are not sorted")
        previous_path = path
        files[path] = item
    return files


def _load_inventory(path: str | Path) -> tuple[dict[str, Any], bytes]:
    inventory_path = Path(path)
    try:
        mode = inventory_path.lstat().st_mode
    except OSError as exc:
        raise AuditError(f"cannot inspect candidate inventory {inventory_path}: {exc}") from exc
    if not stat.S_ISREG(mode):
        raise AuditError(f"candidate inventory must be a regular non-symlink file: {inventory_path}")
    data = _read_file(inventory_path)
    value = _load_json(data, "candidate inventory")
    _validate_inventory(value)
    return value, data


def _load_reference_inventory(path: str | Path) -> tuple[dict[str, Any], bytes]:
    inventory_path = Path(path)
    try:
        mode = inventory_path.lstat().st_mode
    except OSError as exc:
        raise AuditError(f"cannot inspect reference inventory {inventory_path}: {exc}") from exc
    if not stat.S_ISREG(mode):
        raise AuditError(f"reference inventory must be a regular non-symlink file: {inventory_path}")
    data = _read_file(inventory_path)
    value = _load_json(data, "reference inventory")
    _validate_inventory(value, reference=True)
    return value, data


def _load_manifest(path: str | Path) -> tuple[dict[str, tuple[str, int]], bytes]:
    manifest_path = Path(path)
    try:
        mode = manifest_path.lstat().st_mode
    except OSError as exc:
        raise AuditError(f"cannot inspect reference manifest {manifest_path}: {exc}") from exc
    if not stat.S_ISREG(mode):
        raise AuditError(f"reference manifest must be a regular non-symlink file: {manifest_path}")
    data = _read_file(manifest_path)
    manifest = _load_json(data, "reference conversion manifest")
    if not isinstance(manifest, dict) or manifest.get("complete") is not True:
        raise AuditError("reference conversion manifest must be a complete object")
    entries = manifest.get("entries")
    if not isinstance(entries, dict):
        raise AuditError("reference conversion manifest entries must be an object")
    outputs: dict[str, tuple[str, int]] = {}
    for entry_key, entry in entries.items():
        if not isinstance(entry_key, str) or not isinstance(entry, dict):
            raise AuditError("reference conversion manifest contains a malformed entry")
        output = _validate_relative_path(entry.get("output"), f"manifest entry {entry_key!r}")
        _valid_digest(entry.get("output_hash"), f"manifest entry {entry_key!r} output_hash")
        size = entry.get("output_size")
        if type(size) is not int or size < 0:
            raise AuditError(f"manifest entry {entry_key!r} has invalid output_size")
        if output in outputs:
            raise AuditError(f"duplicate conversion manifest output {output!r}")
        outputs[output] = (entry["output_hash"], size)
    return outputs, data


def _manifest_source_path(path: str) -> str:
    if path.endswith(ALIAS_SUFFIX):
        return path[: -len(ALIAS_SUFFIX)] + ".ktx2"
    return path


def _path_in_root(root: Path, relative_path: str) -> Path:
    _validate_relative_path(relative_path, "reference asset")
    current = root
    parts = PurePosixPath(relative_path).parts
    for index, part in enumerate(parts):
        current = current / part
        try:
            mode = current.lstat().st_mode
        except OSError as exc:
            raise AuditError(f"cannot inspect reference asset {relative_path}: {exc}") from exc
        if stat.S_ISLNK(mode):
            raise AuditError(f"symlink in reference asset path {relative_path}")
        if index + 1 < len(parts) and not stat.S_ISDIR(mode):
            raise AuditError(f"non-directory component in reference asset path {relative_path}")
        if index + 1 == len(parts) and not stat.S_ISREG(mode):
            raise AuditError(f"reference asset is not a regular file: {relative_path}")
    return current


def reference_inventory(
    reference_root: str | Path,
    candidate_inventory_path: str | Path,
    manifest_path: str | Path,
) -> dict[str, Any]:
    candidate_document, candidate_raw = _load_inventory(candidate_inventory_path)
    candidate_files = _validate_inventory(candidate_document)
    root = _resolved_root(reference_root)
    manifest_outputs, manifest_raw = _load_manifest(manifest_path)
    files: list[dict[str, Any]] = []

    for path in sorted(candidate_files):
        source = _path_in_root(root, path)
        entry = _inventory_entry(path, source)
        manifest_path_key = _manifest_source_path(path)
        manifest_record = manifest_outputs.get(manifest_path_key)
        if path == RUNTIME_PATH and manifest_record is None:
            provenance = {"kind": "baseline-self-supplemental", "output": path}
        elif manifest_record is None:
            raise AuditError(f"reference manifest has no output entry for {manifest_path_key}")
        else:
            expected_hash, expected_size = manifest_record
            if (entry["sha256"], entry["size"]) != (expected_hash, expected_size):
                raise AuditError(
                    f"reference bytes do not match manifest output {manifest_path_key}"
                )
            provenance = {
                "kind": "conversion-manifest",
                "output": manifest_path_key,
                "output_sha256": expected_hash,
                "output_size": expected_size,
            }
        entry["provenance"] = provenance
        files.append(entry)

    return {
        "format": REFERENCE_FORMAT,
        "candidate_inventory_sha256": _sha256(candidate_raw),
        "manifest_sha256": _sha256(manifest_raw),
        "files": files,
    }


def compare(
    candidate_inventory_path: str | Path,
    reference_inventory_path: str | Path,
) -> dict[str, Any]:
    candidate_document, candidate_raw = _load_inventory(candidate_inventory_path)
    candidate_files = _validate_inventory(candidate_document)
    reference_document, reference_raw = _load_reference_inventory(reference_inventory_path)
    reference_files = _validate_inventory(reference_document, reference=True)

    result: dict[str, Any] = {
        "format": COMPARISON_FORMAT,
        "passed": False,
        "method": {
            "glb": "canonical structured JSON after removing only scenes[0].extras.openSkyrimCollision, plus byte-exact post-JSON data",
            "non_glb": "exact file size and SHA-256",
            "collision_annotation": "candidate-only addition is permitted; existing reference values must remain byte-canonical identical",
            "scope": "portable inventory comparison only; collision correctness and collision audit support are unverified",
        },
        "source_inventories": {
            "candidate_inventory_sha256": _sha256(candidate_raw),
            "reference_inventory_sha256": _sha256(reference_raw),
            "reference_manifest_sha256": reference_document["manifest_sha256"],
        },
        "path_set": {
            "candidate_count": len(candidate_files),
            "reference_count": len(reference_files),
            "candidate_only": sorted(set(candidate_files) - set(reference_files)),
            "reference_only": sorted(set(reference_files) - set(candidate_files)),
        },
        "files": [],
        "errors": [],
    }
    errors: list[str] = result["errors"]
    if reference_document["candidate_inventory_sha256"] != _sha256(candidate_raw):
        errors.append("reference inventory was generated for a different candidate inventory")

    for path in sorted(set(candidate_files) & set(reference_files)):
        candidate_item = candidate_files[path]
        reference_item = reference_files[path]
        kind = candidate_item["kind"]
        issues: list[str] = []
        if kind != reference_item["kind"]:
            issues.append("candidate and reference asset kinds differ")
        candidate_raw = (candidate_item["sha256"], candidate_item["size"])
        reference_raw = (reference_item["sha256"], reference_item["size"])
        candidate_glb = candidate_item.get("glb")
        reference_glb = reference_item.get("glb")
        if kind == "glb" and reference_item["kind"] == "glb":
            if (
                candidate_glb["normalized_json_sha256"]
                != reference_glb["normalized_json_sha256"]
            ):
                issues.append("normalized structured GLB JSON differs")
            if (
                candidate_glb["post_json_sha256"] != reference_glb["post_json_sha256"]
                or candidate_glb["post_json_size"] != reference_glb["post_json_size"]
            ):
                issues.append("bytes or chunks after the GLB JSON chunk differ")
            reference_has_collision = reference_glb["collision_annotation_present"]
            candidate_has_collision = candidate_glb["collision_annotation_present"]
            if reference_has_collision and not candidate_has_collision:
                issues.append("candidate removed a reference collision annotation")
            if (
                reference_has_collision
                and candidate_has_collision
                and candidate_glb["collision_annotation_sha256"]
                != reference_glb["collision_annotation_sha256"]
            ):
                issues.append("candidate changed an existing reference collision annotation")
            if (
                candidate_glb["render_identity_sha256"]
                != reference_glb["render_identity_sha256"]
            ):
                issues.append("normalized GLB render identities differ")
            candidate_normalized: dict[str, Any] | None = candidate_glb
            reference_normalized: dict[str, Any] | None = reference_glb
        else:
            candidate_normalized = None
            reference_normalized = None
            if candidate_raw != reference_raw:
                issues.append("non-GLB bytes differ")
        file_result = {
            "path": path,
            "kind": kind,
            "candidate_raw": {"sha256": candidate_raw[0], "size": candidate_raw[1]},
            "reference_raw": {"sha256": reference_raw[0], "size": reference_raw[1]},
            "candidate_normalized": candidate_normalized,
            "reference_normalized": reference_normalized,
            "reference_provenance": reference_item["provenance"],
            "issues": issues,
        }
        result["files"].append(file_result)
        errors.extend(f"{path}: {issue}" for issue in issues)

    if result["path_set"]["candidate_only"]:
        errors.append("candidate contains installed asset paths absent from the reference")
    if result["path_set"]["reference_only"]:
        errors.append("reference contains installed asset paths absent from the candidate")
    result["passed"] = not errors
    return result


def _emit(value: Any) -> None:
    sys.stdout.write(
        json.dumps(value, ensure_ascii=False, allow_nan=False, sort_keys=True, indent=2) + "\n"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    inventory_parser = commands.add_parser("inventory", help="inventory a candidate installed root")
    inventory_parser.add_argument("candidate_root")
    reference_parser = commands.add_parser(
        "reference-inventory", help="fingerprint candidate paths from a reference root"
    )
    reference_parser.add_argument("reference_root")
    reference_parser.add_argument("--candidate-inventory", required=True)
    reference_parser.add_argument("--manifest", required=True)
    compare_parser = commands.add_parser(
        "compare", help="compare portable candidate and reference inventories"
    )
    compare_parser.add_argument("--candidate-inventory", required=True)
    compare_parser.add_argument("--reference-inventory", required=True)
    args = parser.parse_args(argv)

    try:
        if args.command == "inventory":
            value = inventory(args.candidate_root)
            _emit(value)
            return 0
        if args.command == "reference-inventory":
            value = reference_inventory(
                args.reference_root,
                args.candidate_inventory,
                args.manifest,
            )
            _emit(value)
            return 0
        result = compare(args.candidate_inventory, args.reference_inventory)
        _emit(result)
        return 0 if result["passed"] else 1
    except AuditError as exc:
        _emit({"format": COMPARISON_FORMAT, "passed": False, "errors": [str(exc)]})
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
