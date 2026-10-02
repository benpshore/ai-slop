#!/bin/bash
set -euo pipefail

package_root=$(cd "$(dirname "$0")/.." && pwd)
destination=${1:-"$package_root/build"}
app="$destination/PDFTextract Preview.app"
snapshot="$destination/workspace.png"
mkdir -p "$destination"
# A custom defaults suite is unnecessary: CI is disposable, and the smoke path
# only opens built-in samples. No file access, network access or extraction.
PDFTEXTRACT_UI_SMOKE_SNAPSHOT="$snapshot" "$app/Contents/MacOS/PDFTextractPreview" &
app_pid=$!
trap 'kill "$app_pid" 2>/dev/null || true' EXIT
for ((attempt=0; attempt<30; attempt++)); do
    if ! kill -0 "$app_pid" 2>/dev/null; then
        wait "$app_pid"
        test -s "$snapshot"
        /usr/bin/sips -g pixelWidth -g pixelHeight "$snapshot"
        exit 0
    fi
    sleep 1
done
echo 'GUI smoke test did not finish within 30 seconds.' >&2
exit 1
