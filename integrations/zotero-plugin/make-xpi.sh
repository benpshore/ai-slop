#!/bin/sh
# Package the plugin as an .xpi (a plain zip). No build step.
# Usage: ./make-xpi.sh [output.xpi]   (default: dist/tpe-zotero.xpi next to this script)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-"$here/dist/tpe-zotero.xpi"}
case "$out" in
  /*) ;;
  *) out="$PWD/$out" ;;
esac
mkdir -p "$(dirname "$out")"
rm -f "$out"
cd "$here"
# The version is the git tag (AGENTS.md); the checked-in manifest carries a
# 0.0.0 placeholder that is replaced at packaging time.
ver=${TPE_VERSION:-$(git describe --tags --abbrev=0 2>/dev/null | sed 's/^v//')}
ver=${ver:-0.0.0}
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp -R bootstrap.js prefs.js locale "$stage/"
sed "s/\"version\": \"0.0.0\"/\"version\": \"$ver\"/" manifest.json > "$stage/manifest.json"
grep -q "\"version\": \"$ver\"" "$stage/manifest.json"
(cd "$stage" && zip -q -X -r "$out" manifest.json bootstrap.js prefs.js locale)
echo "$out ($ver)"
