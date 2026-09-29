#!/bin/bash
set -euo pipefail
app=${1:?usage: validate-bundle.sh TPE.app}
appex="$app/Contents/PlugIns/TPEShare.appex"
test "$(plutil -extract LSMinimumSystemVersion raw "$app/Contents/Info.plist")" = 15.0
test "$(plutil -extract NSExtension.NSExtensionPointIdentifier raw "$appex/Contents/Info.plist")" = com.apple.share-services
for version_key in CFBundleShortVersionString CFBundleVersion; do
  test "$(plutil -extract "$version_key" raw "$app/Contents/Info.plist")" = \
    "$(plutil -extract "$version_key" raw "$appex/Contents/Info.plist")"
done
activation=$(plutil -extract NSExtension.NSExtensionAttributes.NSExtensionActivationRule raw "$appex/Contents/Info.plist")
grep -q 'UTI-CONFORMS-TO "com.adobe.pdf"' <<<"$activation"
codesign --verify --deep --strict --verbose=2 "$app"
for bundle in "$app" "$appex"; do
  codesign -dvv "$bundle" 2>&1 | grep -q 'runtime'
  entitlements=$(mktemp)
  codesign -d --entitlements :- "$bundle" >"$entitlements" 2>/dev/null
  test "$(plutil -extract com.apple.security.app-sandbox raw "$entitlements")" = true
  plutil -extract com.apple.security.application-groups xml1 -o - "$entitlements" | grep -q group.org.textprocessingengine.shared
  rm "$entitlements"
done
test "$(plutil -extract com.apple.security.files.user-selected.read-only raw <(codesign -d --entitlements :- "$app" 2>/dev/null))" = true
assessment=$(spctl --assess --type execute --verbose=2 "$app" 2>&1 || true)
grep -q 'rejected' <<<"$assessment" # expected for an ad-hoc CI artifact
