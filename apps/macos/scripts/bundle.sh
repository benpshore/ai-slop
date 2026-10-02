#!/bin/bash
set -euo pipefail

package_root=$(cd "$(dirname "$0")/.." && pwd)
repository_root=$(git -C "$package_root" rev-parse --show-toplevel)
destination=${1:-"$package_root/build"}
tag=$(git -C "$repository_root" describe --tags --abbrev=0)
version=${tag#v}
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Expected a numeric vMAJOR.MINOR.PATCH repository tag, got: $tag" >&2
    exit 1
fi
revision=$(git -C "$repository_root" rev-parse HEAD)
build_number=$(git -C "$repository_root" rev-list --count HEAD)

swift build --package-path "$package_root" --configuration release -Xswiftc -warnings-as-errors
binary_dir=$(swift build --package-path "$package_root" --configuration release --show-bin-path)
app="$destination/PDFTextract Preview.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary_dir/PDFTextractPreview" "$app/Contents/MacOS/PDFTextractPreview"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>PDFTextractPreview</string>
<key>CFBundleIdentifier</key><string>org.pdftextract.preview</string>
<key>CFBundleName</key><string>PDFTextract Preview</string>
<key>CFBundleDisplayName</key><string>PDFTextract Preview</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$version</string>
<key>CFBundleVersion</key><string>$build_number</string>
<key>PDFTextractRevision</key><string>$revision</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSPrincipalClass</key><string>NSApplication</string>
<key>CFBundleDocumentTypes</key><array><dict>
<key>CFBundleTypeName</key><string>PDF document</string>
<key>CFBundleTypeRole</key><string>Viewer</string>
<key>LSHandlerRank</key><string>Alternate</string>
<key>LSItemContentTypes</key><array><string>com.adobe.pdf</string></array>
</dict></array>
</dict></plist>
PLIST
/usr/bin/plutil -lint "$app/Contents/Info.plist"
/usr/bin/codesign --force --sign - "$app"
/usr/bin/codesign --verify --strict "$app"
printf '%s\n' "$app"
