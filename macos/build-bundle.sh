#!/bin/bash
set -euo pipefail

# This script intentionally uses only tools shipped in the selected Xcode.
# CI selects and verifies Xcode 26.0 (17A324), Swift 6.2 and macOS SDK 26.0.
root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-"$root/build/macos"}
sdk=$(xcrun --sdk macosx --show-sdk-path)
arch=$(uname -m)
target="$arch-apple-macos15.0"
modules="$out/modules"
app="$out/TPE.app"
appex="$app/Contents/PlugIns/TPEShare.appex"
rm -rf "$out"
mkdir -p "$modules" "$app/Contents/MacOS" "$app/Contents/PlugIns" "$appex/Contents/MacOS"

xcrun swiftc -swift-version 6 -sdk "$sdk" -target "$target" -O \
  -emit-library -static -emit-module -module-name TPEMacSupport \
  -emit-module-path "$modules/TPEMacSupport.swiftmodule" \
  "$root/macos/Shared/SharedInbox.swift" -o "$modules/libTPEMacSupport.a"

xcrun swiftc -swift-version 6 -sdk "$sdk" -target "$target" -O -parse-as-library \
  -I "$modules" -L "$modules" -lTPEMacSupport \
  "$root/macos/TPEApp/TPEApp.swift" "$root/macos/TPEApp/IntakeModel.swift" \
  "$root/macos/TPEApp/DashboardView.swift" -o "$app/Contents/MacOS/TPE"

xcrun swiftc -swift-version 6 -sdk "$sdk" -target "$target" -O -application-extension \
  -emit-library -I "$modules" -L "$modules" -lTPEMacSupport \
  "$root/macos/ShareExtension/ShareViewController.swift" -o "$appex/Contents/MacOS/TPEShare"

cp "$root/macos/Configuration/App-Info.plist" "$app/Contents/Info.plist"
cp "$root/macos/Configuration/Share-Info.plist" "$appex/Contents/Info.plist"
for info_plist in "$app/Contents/Info.plist" "$appex/Contents/Info.plist"; do
  plutil -replace CFBundleShortVersionString -string "${TPE_MARKETING_VERSION:-0.0.0}" "$info_plist"
  plutil -replace CFBundleVersion -string "${GITHUB_RUN_NUMBER:-1}" "$info_plist"
done

# Ad-hoc signing makes local/CI launch tests possible and proves nested signing
# order. Release automation replaces '-' with a Developer ID identity and adds
# --timestamp; notarization is deliberately never claimed for ad-hoc builds.
codesign --force --sign - --options runtime --entitlements "$root/macos/Configuration/Share.entitlements" "$appex"
codesign --force --sign - --options runtime --entitlements "$root/macos/Configuration/App.entitlements" "$app"
printf '%s\n' "$app"
