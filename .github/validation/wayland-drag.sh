#!/usr/bin/env bash
set -euo pipefail

log_dir="$RUNNER_TEMP/winit-wayland-drag"
mkdir -p "$log_dir"
export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR="$(mktemp -d)"
export WAYLAND_DISPLAY=winit-drag-test
export LIBGL_ALWAYS_SOFTWARE=1
export MUTTER_DEBUG_DUMMY_MODE_SPECS=1280x800
mutter --wayland --nested --no-x11 --sm-disable --wayland-display="$WAYLAND_DISPLAY" \
  >"$log_dir/mutter.log" 2>&1 &
compositor=$!
trap 'kill "$compositor" 2>/dev/null || true' EXIT
for _ in {1..50}; do
  if [[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]]; then break; fi
  kill -0 "$compositor"
  sleep 0.2
done
[[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]]
/usr/bin/python3 .github/validation/wayland-drag.py "$log_dir" target/debug/examples/wayland_internal_drag
