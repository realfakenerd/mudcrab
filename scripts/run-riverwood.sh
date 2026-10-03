#!/bin/sh
# Fiji Riverwood package launcher; package reports specify schema and test scope.
# Bundled models and textures cover a six-cell radius around grid (5, -12).
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ ! -r "$dir/lib/libdl.so.2" ]; then
  echo 'Riverwood package incomplete: missing lib/libdl.so.2 required for Vulkan loading.' >&2
  exit 1
fi
export LD_LIBRARY_PATH="$dir/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# The bundled ALSA library loads its config hook from ./lib/libasound.so.2
# relative to this root. Vulkan discovers the host's GPU driver itself.
export ALSA_PLUGIN_DIR="$dir"
# Terminals opened through SSH or tmux can lack the graphical session's display
# variables even while the same user has an active Wayland compositor.
if [ -z "${XDG_RUNTIME_DIR:-}" ] && [ -d "/run/user/$(id -u)" ]; then
  XDG_RUNTIME_DIR="/run/user/$(id -u)"
  export XDG_RUNTIME_DIR
fi
if [ -z "${WAYLAND_DISPLAY:-}" ] && [ -z "${WAYLAND_SOCKET:-}" ] && [ -z "${DISPLAY:-}" ]; then
  for socket in "${XDG_RUNTIME_DIR:-/nonexistent}"/wayland-*; do
    [ -S "$socket" ] || continue
    name=${socket##*/}
    number=${name#wayland-}
    case "$number" in ''|*[!0-9]*) continue ;; esac
    WAYLAND_DISPLAY=$name
    export WAYLAND_DISPLAY
    break
  done
  if [ -z "${WAYLAND_DISPLAY:-}" ]; then
    echo 'No Wayland display found; start an active Fiji desktop session or set WAYLAND_DISPLAY.' >&2
    exit 1
  fi
fi
# Bundled libxkbcommon bakes in the build host's xkeyboard-config path; point
# it at the target machine's xkb data.
if [ -z "${XKB_CONFIG_ROOT:-}" ]; then
  for candidate in /nix/store/*-xkeyboard-config-*/share/X11/xkb /usr/share/X11/xkb /usr/local/share/X11/xkb; do
    if [ -d "$candidate" ]; then
      XKB_CONFIG_ROOT="$candidate"
      break
    fi
  done
fi
if [ ! -d "${XKB_CONFIG_ROOT:-}" ]; then
  echo 'No keyboard layout data found; set XKB_CONFIG_ROOT to an installed X11/xkb directory.' >&2
  exit 1
fi
export XKB_CONFIG_ROOT
exec "$dir/lib/ld-linux-x86-64.so.2" --library-path "$dir/lib" "$dir/bin/engine" \
  --assets "$dir/assets" --worldspace 60 --grid-x 5 --grid-y -12 --stream-radius 2 "$@"
