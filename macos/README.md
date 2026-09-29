# macOS application packaging

Run `macos/package.sh`. It builds the GPUI binary, creates the standard
`Contents/MacOS` and `Contents/Resources` layout, produces the multi-resolution
icon, substitutes the tag-derived version, and signs and validates the bundle.
Set `CODESIGN_IDENTITY` to a Developer ID Application identity for distribution;
the default is an ad-hoc signature for local integration testing.

The app declares `com.adobe.pdf` as a Viewer. GPUI's native application delegate
forwards `application:openURLs:` and reopen events through `Application::on_open_urls`
and `Application::on_reopen`; the same `DocumentIntake` receives Finder, command-line,
and window drop submissions.

The sandbox grants read-only access only to URLs explicitly selected by the user.
Watched directories must be chosen using `NSOpenPanel` and stored as
security-scoped bookmark data (never their parent or a broad filesystem grant).
Resolve stale bookmarks and replace them after the user reselects a moved folder;
always balance `startAccessingSecurityScopedResource()` with `stopAccessing…`.
