#!/usr/bin/env bash
set -euo pipefail

package=$(realpath -- "${1:?usage: capture-riverwood-lod.sh PACKAGE [OUTPUT]}")
output=$(realpath -m -- "${2:-"$(dirname -- "$package")/riverwood-lod-capture-$(date -u +%Y%m%dT%H%M%SZ)"}")
[[ "$output" != "$package" && "$output" != "$package/"* ]] || {
    printf 'Capture output must be outside the package: %s\n' "$output" >&2; exit 2;
}
[[ ! -e "$output" ]] || { printf 'Capture output already exists: %s\n' "$output" >&2; exit 2; }
[[ -x "$package/run-riverwood-lod.sh" ]] || { printf 'Missing LOD launcher\n' >&2; exit 2; }
[[ -r "$package/build-provenance.json" ]] || { printf 'Missing build provenance\n' >&2; exit 2; }
readarray -t provenance < <(python3 - "$package" <<'PY'
import hashlib, json, pathlib, re, sqlite3, sys
root = pathlib.Path(sys.argv[1])
build = json.loads((root / "build-provenance.json").read_text())
if type(build.get("format_version")) is not int or build["format_version"] != 1:
    raise SystemExit("Unsupported build provenance")
if not isinstance(build.get("commit"), str) or not re.fullmatch(r"[0-9a-f]{40}", build["commit"]):
    raise SystemExit("Invalid build commit")
if type(build.get("dirty_worktree")) is not bool:
    raise SystemExit("Invalid dirty_worktree flag")
checksums = build.get("checksums")
if not isinstance(checksums, dict):
    raise SystemExit("Missing build checksums")
for relative in (
    "bin/engine", "assets/skyrim_world.db", "assets/cell_cache.rkyv",
    "assets/lod-manifest.json", "assets/conversion-manifest.json",
    "assets/integration-report.json",
):
    expected = checksums.get(relative)
    if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{64}", expected):
        raise SystemExit(f"Invalid build checksum: {relative}")
    digest = hashlib.sha256()
    with (root / relative).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    if digest.hexdigest() != expected:
        raise SystemExit(f"Build checksum mismatch: {relative}")
manifest = json.loads((root / "assets/lod-manifest.json").read_text())
integration = json.loads((root / "assets/integration-report.json").read_text())
if manifest.get("converter_schema") != 17 or manifest.get("world_database_schema") != 5:
    raise SystemExit("Unsupported LOD schemas")
if integration.get("schema_version") != 5 or integration.get("passed") is not True:
    raise SystemExit("Asset integration did not pass")
identity = build.get("lod_build_identity")
if not isinstance(identity, str) or not re.fullmatch(r"[0-9a-f]{64}", identity):
    raise SystemExit("Invalid LOD build identity")
with sqlite3.connect((root / "assets/skyrim_world.db").as_uri() + "?mode=ro", uri=True) as database:
    schema = database.execute("SELECT version FROM schema_info").fetchall()
    database_identity = database.execute("SELECT build_identity FROM lod_build WHERE id=1").fetchone()
if schema != [(5,)] or database_identity != (identity,) or manifest.get("build_identity") != identity:
    raise SystemExit("Database and manifest LOD identities differ")
print(build["commit"])
print(str(build["dirty_worktree"]).lower())
PY
)
[[ ${#provenance[@]} == 2 && ${provenance[0]} =~ ^[0-9a-f]{40}$ ]] || {
    printf 'Invalid build provenance\n' >&2; exit 2;
}
[[ -r /run/opengl-driver/share/vulkan/icd.d/radeon_icd.x86_64.json ]] || {
    printf 'Fiji Radeon ICD is unavailable\n' >&2; exit 2;
}
mkdir -- "$output"
output=$(realpath -- "$output")
cp -- "$package/build-provenance.json" "$output/build-provenance.json"
uname -srm >"$output/hardware.txt"
sha256sum "$package/bin/engine" "$package/assets/skyrim_world.db" \
    "$package/assets/lod-manifest.json" >"$output/input-checksums.txt"
unset WGPU_FORCE_FALLBACK_ADAPTER

capture() {
    local name=$1 radius=$2 status=0
    local -a flags=(
        --stream-radius "$radius" --benchmark-duration 30 --benchmark-warmup-frames 120
        --benchmark-output "$output/$name-benchmark.json"
        --benchmark-frame-times "$output/$name-frame-times.csv"
        --acceptance-screenshot "$output/$name.png"
        --screenshot-camera-offset "0,6000,8000"
        --profile-output "$output/$name-profile" --profile-scenario "$name"
        --profile-run-id capture-1 --profile-commit "${provenance[0]}"
        --profile-hardware "$(cat "$output/hardware.txt")"
        --run-label "$name"
    )
    if [[ ${provenance[1]} == true ]]; then flags+=(--profile-dirty-worktree); fi
    "$package/run-riverwood-lod.sh" "${flags[@]}" >"$output/$name.log" 2>&1 || status=$?
    printf '%s\n' "$status" >"$output/$name-exit-code.txt"
}

# Same camera, different full-cell coverage: handoff smoke, not an equal-quality
# benchmark. Native engine performance thresholds remain unchanged.
capture riverwood-radius2 2
capture riverwood-radius0 0

python3 - "$output" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
errors = []
for name in ("riverwood-radius2", "riverwood-radius0"):
    if int((root / f"{name}-exit-code.txt").read_text()) != 0:
        errors.append(f"{name}: engine acceptance failed")
    if not (root / f"{name}.png").is_file():
        errors.append(f"{name}: no ready-state screenshot")
    try:
        benchmark = json.loads((root / f"{name}-benchmark.json").read_text())
        if benchmark.get("passed") is not True or benchmark.get("scenario") != name:
            errors.append(f"{name}: benchmark acceptance did not pass")
    except (OSError, ValueError):
        errors.append(f"{name}: missing or invalid benchmark report")
    path = root / f"{name}-profile" / "streaming.json"
    if not path.is_file():
        errors.append(f"{name}: no streaming profile")
        continue
    metrics = json.loads(path.read_text()).get("aggregate") or {}
    for key in (
        "pending_lod_chunks", "failed_lod_chunks", "pending_lod_queries",
        "failed_lod_queries", "failed_cells", "loading_cells",
        "pending_asset_instances", "pending_surface_instances", "asset_load_failures",
        "material_validation_failures", "streaming_invariant_failures", "diagnostic_fallbacks",
    ):
        if metrics.get(key) != 0:
            errors.append(f"{name}: {key}={metrics.get(key)!r}")
    if not metrics.get("lod_chunks_ready", 0) or not metrics.get("visible_lod_terrain_patches", 0):
        errors.append(f"{name}: no visible ready terrain LOD")
report = {"scope": "Fiji Riverwood terrain handoff smoke; unequal full-cell coverage", "passed": not errors, "errors": errors}
(root / "capture-report.json").write_text(json.dumps(report, indent=2) + "\n")
if errors:
    raise SystemExit("\n".join(errors))
PY
printf 'Riverwood captures: %s\n' "$output"
