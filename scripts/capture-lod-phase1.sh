#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
stamp=$(date -u +%Y%m%dT%H%M%SZ)
output=${1:-"$HOME/.cache/openskyrim/lod-phase1-$stamp"}
if [[ -e "$output" ]] && [[ -n "$(find "$output" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
    printf 'Refusing non-empty output directory: %s\n' "$output" >&2
    exit 2
fi
mkdir -p "$output"

scratch_root=${TMPDIR:-/dev/shm}
mkdir -p "$scratch_root"
scratch=$(mktemp -d "$scratch_root/openskyrim-lod-capture.XXXXXX")
cleanup() {
    rm -rf -- "$scratch"
}
trap cleanup EXIT

data="$scratch/Data"
assets="$scratch/Assets"
mkdir -p "$data"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo/target}
export TMPDIR=${TMPDIR:-$HOME/.cache/openskyrim-lod-tmp}
mkdir -p "$TMPDIR"
# winit dlopens libxkbcommon-x11 at window creation and wgpu dlopens
# libvulkan at renderer init; NixOS keeps both out of the default loader
# path, so expose the devenv-provided copies.
add_library_dir() {
    case ":${LD_LIBRARY_PATH:-}:" in
    *":$1:"*) ;;
    *) export LD_LIBRARY_PATH="${LD_LIBRARY_PATH:+$LD_LIBRARY_PATH:}$1" ;;
    esac
}
for candidate in /nix/store/*-libxkbcommon-*/lib/libxkbcommon-x11.so.0; do
    if [[ -f "$candidate" ]]; then
        add_library_dir "$(dirname "$candidate")"
        break
    fi
done
for candidate in /nix/store/*-vulkan-loader-*/lib/libvulkan.so.1; do
    if [[ -f "$candidate" ]]; then
        add_library_dir "$(dirname "$candidate")"
        break
    fi
done
unset -f add_library_dir
# Headless software rendering: no /dev/dri on this host, so point the Vulkan
# loader at lavapipe and let wgpu accept the CPU fallback adapter.
if [[ -z "${VK_ICD_FILENAMES:-}" ]]; then
    # Prefer mesa 25.x lavapipe: wgpu 29 device creation fails against the
    # 26.x ICD on this host (vkCreateDevice ERROR_FEATURE_NOT_PRESENT).
    for candidate in /nix/store/*-mesa-25.*/share/vulkan/icd.d/lvp_icd.*.json \
        /nix/store/*-mesa-*/share/vulkan/icd.d/lvp_icd.*.json; do
        if [[ -f "$candidate" ]]; then
            export VK_ICD_FILENAMES="$candidate"
            break
        fi
    done
fi
export WGPU_FORCE_FALLBACK_ADAPTER="${WGPU_FORCE_FALLBACK_ADAPTER:-1}"

commit=$(git -C "$repo" rev-parse HEAD)
git -C "$repo" status --porcelain=v1 >"$output/source-status.txt"
git -C "$repo" diff --binary HEAD >"$output/source-diff.patch"
dirty=false
if [[ -s "$output/source-status.txt" ]]; then
    dirty=true
fi

cargo run --locked --manifest-path "$repo/Cargo.toml" -p dummy-content -- gen "$data" \
    >"$output/dummy-content.log" 2>&1
cargo run --locked --manifest-path "$repo/Cargo.toml" -p converter --bin converter -- "$data" "$assets" \
    --cpu-jobs "${LOD_CONVERTER_JOBS:-4}" --io-jobs 2 >"$output/converter.log" 2>&1

hardware=$(uname -srm)
capture() {
    local name=$1 radius=$2
    local screenshot="$output/$name.png"
    local profile="$output/$name-profile"
    local log="$output/$name.log"
    local -a command=(
        cargo run --locked --manifest-path "$repo/Cargo.toml" -p engine --bin engine --
        --assets "$assets" --worldspace 1 --grid-x 0 --grid-y 0
        --stream-radius "$radius" --benchmark-duration 8
        --benchmark-warmup-frames 0 --benchmark-output "$output/$name-benchmark.json"
        --accept-min-fps 0 --accept-p95-ms 100000
        --accept-max-memory-growth-gib 1000
        --acceptance-screenshot "$screenshot"
        --screenshot-camera-offset "0,6000,8000"
        --profile-output "$profile" --profile-scenario "$name"
        --profile-run-id capture-1 --profile-commit "$commit"
        --profile-hardware "$hardware"
    )
    if [[ "$dirty" == true ]]; then
        command+=(--profile-dirty-worktree)
    fi
    if [[ -z ${DISPLAY:-} ]]; then
        command -v xvfb-run >/dev/null || {
            printf 'xvfb-run is required when DISPLAY is unset; run inside devenv.\n' >&2
            exit 2
        }
        xvfb-run -a "${command[@]}" >"$log" 2>&1
    else
        "${command[@]}" >"$log" 2>&1
    fi
    [[ -s "$screenshot" ]] || {
        printf 'Screenshot was not captured: %s (see %s)\n' "$screenshot" "$log" >&2
        exit 1
    }
    [[ -s "$profile/streaming.json" ]] || {
        printf 'Streaming profile was not written: %s\n' "$profile" >&2
        exit 1
    }
}

# Full-detail radius 1 covers exactly the 3x3 GeneratedWorld fixture; radius 2
# would request 16 nonexistent cells, which count as failed_cells and block
# the screenshot gate.
capture full-detail 1
capture terrain-lod 0

python3 - "$output/terrain-lod-profile/streaming.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    profile = json.load(stream)
metrics = profile.get("aggregate") or {}
required = {
    "lod_chunks_ready": lambda value: value > 0,
    "visible_lod_terrain_patches": lambda value: value > 0,
    "failed_lod_chunks": lambda value: value == 0,
    "pending_lod_chunks": lambda value: value == 0,
    "failed_lod_queries": lambda value: value == 0,
    "pending_lod_queries": lambda value: value == 0,
}
for key, predicate in required.items():
    value = metrics.get(key)
    if value is None or not predicate(value):
        raise SystemExit(f"LOD capture gate failed: {key}={value!r}")
PY

{
    printf 'captured_utc=%s\n' "$stamp"
    printf 'commit=%s\n' "$commit"
    printf 'dirty_worktree=%s\n' "$dirty"
    printf 'hardware=%s\n' "$hardware"
    printf 'fixture=deterministic dummy-content GeneratedWorld\n'
    printf 'validation_scope=software-rendered fixture smoke, not target-hardware acceptance\n'
    printf 'scenarios=full-detail,terrain-lod\n'
    sha256sum "$output/source-status.txt" "$output/source-diff.patch"
    sha256sum "$output/full-detail.png" "$output/terrain-lod.png"
} >"$output/capture-manifest.txt"

printf 'LOD Phase 1 captures: %s\n' "$output"
