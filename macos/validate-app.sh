#!/bin/sh
set -eu
APP=${1:?usage: validate-app.sh APPLICATION.app}
PLIST="$APP/Contents/Info.plist"
test -x "$APP/Contents/MacOS/tpe-app"
test -s "$APP/Contents/Resources/AppIcon.icns"
plutil -lint "$PLIST" >/dev/null
[ "$(plutil -extract CFBundlePackageType raw "$PLIST")" = APPL ]
/usr/libexec/PlistBuddy -c 'Print :CFBundleDocumentTypes:0:LSItemContentTypes:0' "$PLIST" | grep -qx com.adobe.pdf
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -d --entitlements :- "$APP" 2>&1 | grep -q com.apple.security.app-sandbox
