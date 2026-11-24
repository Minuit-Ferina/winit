#!/usr/bin/env bash
set -euo pipefail

logs="$RUNNER_TEMP/winit-drag-logs"
mkdir -p "$logs"
swiftc .github/validation/ExternalTarget.swift -o "$RUNNER_TEMP/external-target"
swiftc .github/validation/DragInput.swift -o "$RUNNER_TEMP/drag-input"

"$RUNNER_TEMP/drag-input" --preflight
"$RUNNER_TEMP/external-target" > "$logs/external.log" 2>&1 &
external_pid=$!
RUST_LOG=info target/debug/examples/custom_drag_format > "$logs/demo.log" 2>&1 &
demo_pid=$!
trap 'kill "$demo_pid" "$external_pid" 2>/dev/null || true; cat "$logs/demo.log" "$logs/external.log" 2>/dev/null || true' EXIT

"$RUNNER_TEMP/drag-input" "$demo_pid" "$external_pid"
grep -q 'Received 9 bytes: \[1, 0, 0, 0, 0, 0, 0, 0, 0\]' "$logs/demo.log"
grep -q 'Received 9 bytes: \[4, 0, 0, 0, 0, 0, 0, 0, 0\]' "$logs/demo.log"
[[ $(grep -c 'Received 9 bytes:' "$logs/demo.log") -eq 2 ]]
[[ $(grep -c 'Drag canceled' "$logs/demo.log") -eq 2 ]]
! grep -q 'Foreign drop performed' "$logs/external.log"
echo "AppKit accepted the internal drops, refused the external drop, and canceled Escape."
