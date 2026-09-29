#!/bin/sh
# LaunchServices smoke coverage for the packaged application. The Rust unit
# tests assert queue semantics; this script asserts that macOS can deliver the
# corresponding native events without terminating the app.
set -eu
APP=${1:?usage: integration-test.sh APPLICATION.app}
TMP=$(mktemp -d)
trap 'open -a "$APP" --args --quit-after-test >/dev/null 2>&1 || true; rm -rf "$TMP"' EXIT
printf '%%PDF-1.4\n%%%%EOF\n' > "$TMP/one.pdf"
printf '%%PDF-1.4\n%%%%EOF\n' > "$TMP/two.pdf"

# Finder/Open With delivery and a multiple-file application:openURLs: event.
open -n -a "$APP" "$TMP/one.pdf" "$TMP/two.pdf"
sleep 2
pgrep -f "$APP/Contents/MacOS/tpe-app" >/dev/null
# Reopen an already-running instance, then submit another URL (same intake API).
open -a "$APP"
open -a "$APP" "$TMP/one.pdf"
sleep 1
pgrep -f "$APP/Contents/MacOS/tpe-app" >/dev/null
# Drag-and-drop is exercised at the GPUI event boundary by its ExternalPaths
# handler; UI automation is intentionally left to a signed local test runner.
osascript -e 'tell application "System Events" to exists process "tpe-app"' | grep -q true
pkill -f "$APP/Contents/MacOS/tpe-app"
