# PDFTextract macOS preview

A separate native **SwiftUI + AppKit** application for macOS 14 or later. It
configures and demonstrates the document workflow without connecting to the Rust
engine. Swift 6 / Xcode 16 or later is required; there are no package dependencies.

SwiftUI provides native windowing, menus, keyboard navigation, accessibility,
settings and file panels for this macOS-only app. AppKit supplies the file panels
and clipboard, and PDFKit displays a selected local source PDF. The existing GPUI
app in `crates/tpe-app` remains intact. This is a new application package, not a
restoration of the archived Swift scaffold or a rewrite of the engine.

## What works

- Add local PDFs using the file panel, drag and drop, or Finder's Open With.
- Search and filter the workspace; inspect complete, limited and failed samples.
- Queue, run, cancel, retry and remove preview jobs. Cancellation rejects late
  progress/results, and a retry has a new identity.
- Inspect fictional text and references, warnings and captured job options;
  copy sample text or explicitly export sample JSON.
- View the original PDF locally with PDFKit, independently of extraction.
- Save default output/engine preferences locally. Defaults are captured when a
  document is added or retried. The workspace itself lasts for one app session.

The window continuously labels this as an **offline preview**. All extraction
results are fictional and include `isSimulated: true` and provider identity
`offline-preview/1`; imported files are never used to generate these results.
Limited outcomes remain distinct from complete outcomes, and reference resolution
is marked unmeasured. There are no network requests, HTTP listeners, API keys,
authentication, Rust library calls, worker launches, OCR or real extraction.
Selected PDFs are read only when their Source PDF tab is displayed by PDFKit.

## Build, test and run

From the repository root on a Mac:

```sh
swift test --package-path apps/macos -Xswiftc -warnings-as-errors
swift run --package-path apps/macos PDFTextractPreview
bash apps/macos/scripts/bundle.sh
open 'apps/macos/build/PDFTextract Preview.app'
```

The bundle script derives the display version from the latest repository tag and
records the exact code revision. It creates an ad hoc signed local development
app; it does not provide Developer ID signing, notarization, a hardened runtime,
an App Sandbox entitlement or a distributable release. Build and launch on the
target Mac architecture; no universal binary or Apple Silicon performance claim
is made. Delete this isolated package's `.build` directory if changing Xcode
versions causes stale build products.

```sh
bash apps/macos/scripts/smoke.sh
```

The smoke check launches the bundle, renders the sample workspace into
`apps/macos/build/workspace.png`, then terminates it, with a 30-second deadline.
The path-scoped `macOS GUI preview` workflow builds/tests with warnings as errors,
creates the app, launches it and uploads the app ZIP, screenshot, toolchain and
code SHA. The Linux engine workflow is unchanged. macOS CI does not substitute
for manual VoiceOver, keyboard, drag-and-drop, display scaling and PDFKit testing.

## Connecting the Rust engine later

`WorkspaceCore.EngineClient` is the UI-side injection boundary: a request snapshots
the source and options; progress is asynchronous; a result carries completeness,
warnings, backend identity and identifier evidence. `WorkspaceModel` receives a
client in its initializer. The current `PreviewEngineClient` only returns fixtures.

This protocol is not a proposed wire schema. A later adapter must map the real
Rust API's supported capabilities and errors into the UI, preserve resource-limit
and identifier-verification outcomes, and wire cancellation to actual work. It
must not silently turn unsupported options or incomplete extraction into success.
Native engine functionality and its API should be verified independently before
that adapter is connected. No transport or authentication choice is made here.

The queue/contract tests cover duplicate and remote input rejection, option
snapshots, guarded state transitions, retry identity, cancellation, partial-result
serialization, sample-mode output selection and explicit simulated provenance.
