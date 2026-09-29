# macOS host and Share extension

## Supported platform and pinned toolchain

The native host and Share extension support **macOS 15.0 and later** and are built/tested against **macOS 26**.

| Component | Required validation version |
|---|---|
| macOS runner | macOS 26, Apple silicon (`macos-26`) |
| Xcode | 26.0, build `17A324` |
| macOS SDK | 26.0 |
| Swift | 6.2 compiler; sources use Swift 6 mode |
| Rust GPUI compatibility UI | exactly `gpui 0.2.2` in `Cargo.toml` and `Cargo.lock` |
| Deployment target | macOS 15.0 (Swift packages, compiler target and both bundle plists) |

`.github/workflows/macos-26.yml` selects `/Applications/Xcode_26.0.app/Contents/Developer` and fails before compiling if any version differs. Moving to a point release or SDK is therefore a reviewed dependency update, not an ambient runner-image change. Cargo transitives are locked in `Cargo.lock`; both Swift packages have no external package dependencies. `macos/build-bundle.sh` invokes only the selected Xcode, embeds the extension, and signs inside-out.

The repository version remains tag-derived. Validation uses `0.0.0` and a CI build number; release automation must inject the tag rather than edit a version field.

## SDK 26 lifecycle and data-flow audit

The user-facing intake/status UI is native SwiftUI/AppKit. The pinned legacy GPUI workbench remains available for document detail, but is not the accessible entry point because GPUI 0.2.2 exposes no macOS accessibility tree.

| Area | SDK 26 implementation and invariant | Automated evidence |
|---|---|---|
| Window lifecycle | SwiftUI `WindowGroup`; no manually retained `NSWindow`, app delegate, or activation-policy override. | Bundle launch smoke. |
| Finder/file-open | `CFBundleDocumentTypes` declares `com.adobe.pdf`; `onOpenURL` is the single Apple-event entry point. | `open -a TPE.app fixture.pdf` smoke. |
| Drag and drop | Typed `dropDestination(for: URL.self)` filters PDF extensions and calls the same intake method. | Shared intake unit test and compiler coverage. |
| Uniform Type Identifiers | `UTType.pdf`/its identifier; no deprecated MobileCoreServices constants. | Swift build and provider test. |
| Security-scoped files | Access begins before bookmark/copy and stops with `defer`; bookmarks use `.withSecurityScope`. A private durable copy avoids later dependence on a stale external URL. | Shared inbox tests and entitlements. |
| App Group | One identifier in both entitlements. An atomically replaced plist sits beside a UUID-named PDF in `Inbox`; there is no shared mutable database. | Unit and entitlement checks. |
| Share activation | `com.apple.share-services`, principal class, and a one-item `UTI-CONFORMS-TO "com.adobe.pdf"` predicate; runtime also requires `UTType.pdf`. Completion/cancellation follows async import exactly once. | `NSItemProvider` share-import test and plist checks. |
| Sandbox | Both targets enable App Sandbox and App Group. Only host has user-selected read-only access; neither requests network, Downloads, automation, or exceptions. | Signed-entitlement checks. |
| Hardened runtime | Host and nested extension use `codesign --options runtime`; no JIT/debugger/library-validation exceptions. | `codesign --verify --deep --strict`. |
| Termination | Intake copies synchronously before success; extension completes after copy and atomic metadata write. | Unit/share tests. |

### Signing and notarization

CI uses an ad-hoc identity, validates structure, inside-out signing order, hardened-runtime flags and entitlements, and confirms Gatekeeper rejects the artifact as unnotarized. A distributable build must use Developer ID:

```sh
codesign --force --options runtime --timestamp --entitlements macos/Configuration/Share.entitlements --sign "$DEVELOPER_ID_APPLICATION" TPE.app/Contents/PlugIns/TPEShare.appex
codesign --force --options runtime --timestamp --entitlements macos/Configuration/App.entitlements --sign "$DEVELOPER_ID_APPLICATION" TPE.app
ditto -c -k --keepParent TPE.app TPE.zip
xcrun notarytool submit TPE.zip --keychain-profile TPE-NOTARY --wait
xcrun stapler staple TPE.app
spctl --assess --type execute --verbose=2 TPE.app
```

The App Group must exist in the Developer portal and occur in both provisioning profiles. Release automation must compare extracted entitlements with the checked-in least-privilege files and must not use `--deep` to create signatures (`--deep` here is verification only).

## Accessibility contract

The dashboard is native, so AppKit supplies semantic objects to VoiceOver:

* **Keyboard/full keyboard access:** initial focus is on Open, Return activates it, Command-O is in the File command, and native controls provide Tab/Shift-Tab and list navigation.
* **Names and values:** rows combine into one named element with imported-time value; Open has a visible name/hint; status and progress have explicit labels/values.
* **Announcements:** completed intake posts a high-priority accessibility announcement. Errors beep and remain in the named status.
* **Text scaling:** semantic system text styles follow macOS text size; the decorative glyph grows at accessibility sizes. Text has no fixed point size.
* **Reduced motion:** progress animation is removed when Reduce Motion is enabled.
* **Contrast:** semantic colors and native bordered controls preserve Increase Contrast, Dark Mode and accent behavior. Color is not the only signal.

Before release, run the lane and a physical macOS 26 acceptance pass with VoiceOver and Full Keyboard Access: traverse in reading order, import by Command-O and drop, invoke Share from Preview, hear filename and progress/completion, select every row, enlarge system text, and repeat with Reduce Motion and Increase Contrast. This manual assistive-technology pass is a release gate because XCTest cannot assert spoken output or visually measure system contrast.

## Local validation

```sh
cargo test --workspace
swift test
swift test --package-path macos
./macos/build-bundle.sh
./macos/validate-bundle.sh build/macos/TPE.app
./macos/smoke-tests.sh build/macos/TPE.app
```
