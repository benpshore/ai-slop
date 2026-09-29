#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
OUT=${1:-"$ROOT/dist"}
APP="$OUT/Text Processing Engine.app"
VERSION=${VERSION:-$(git -C "$ROOT" describe --tags --abbrev=0 2>/dev/null | sed 's/^v//' || printf '0.0.0')}
BUILD_NUMBER=$(git -C "$ROOT" rev-list --count HEAD)
: "${CODESIGN_IDENTITY:--}"

cargo build --manifest-path "$ROOT/Cargo.toml" --release -p tpe-app -p text-processing-engine
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/tpe-app" "$APP/Contents/MacOS/tpe-app"
cp "$ROOT/target/release/tpe" "$APP/Contents/MacOS/tpe"
sed -e "s/$(printf '\044')(VERSION)/$VERSION/g" -e "s/$(printf '\044')(BUILD_NUMBER)/$BUILD_NUMBER/g" \
  "$ROOT/macos/Info.plist" > "$APP/Contents/Info.plist"

ICONSET=$(mktemp -d)/AppIcon.iconset
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -s format png -z "$size" "$size" "$ROOT/macos/AppIcon.svg" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -s format png -z "$double" "$double" "$ROOT/macos/AppIcon.svg" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
codesign --force --options runtime --timestamp=none --entitlements "$ROOT/macos/TPE.entitlements" \
  --sign "$CODESIGN_IDENTITY" "$APP/Contents/MacOS/tpe"
codesign --force --options runtime --timestamp=none --entitlements "$ROOT/macos/TPE.entitlements" \
  --sign "$CODESIGN_IDENTITY" "$APP"
"$ROOT/macos/validate-app.sh" "$APP"
printf '%s\n' "$APP"
