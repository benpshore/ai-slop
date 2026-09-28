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
zip -q -X -r "$out" manifest.json bootstrap.js prefs.js locale
echo "$out"
