#!/bin/sh
# Assemble target/release/PDFTextract.app from the release `tpe-app` binary,
# ad-hoc signed. macOS only. Usage: sh crates/tpe-app/bundle.sh [--open]
#
# VERSION defaults to the latest v* tag (the release version scheme) and the
# bundle build number is the short commit; bundle/Info.plist holds the rest.
# The same VERSION is compiled into the binary (`--version`) so it agrees
# with the bundle.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
VERSION=${VERSION:-$(git -C "$ROOT" describe --tags --match 'v*' --abbrev=0 2>/dev/null | sed 's/^v//')}
VERSION=${VERSION:-0.0.0}
BUILD=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo dev)
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}

(cd "$ROOT" && PDFTEXTRACT_VERSION="$VERSION" cargo build --release --locked -p tpe-app)

APP="$TARGET/release/PDFTextract.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET/release/tpe-app" "$APP/Contents/MacOS/PDFTextract"
sed -e "s/__VERSION__/$VERSION/" -e "s/__BUILD__/$BUILD/" "$HERE/bundle/Info.plist" > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
plutil -lint "$APP/Contents/Info.plist"
codesign --force --sign - "$APP"
codesign --verify --strict "$APP"
test "$("$APP/Contents/MacOS/PDFTextract" --version)" = "PDFTextract $VERSION"
echo "built $APP ($VERSION, $BUILD)"
if [ "${1:-}" = "--open" ]; then open "$APP"; fi
