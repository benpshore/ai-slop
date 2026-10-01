# Upstream integration and update policy

Research checked **2026-09-28**. These are upstream findings, not results measured in this repository. Recheck the selected revisions during implementation. Research snapshot SHAs are not validated release pins.

## Components

| Component | Primary sources | Integration consequence |
| --- | --- | --- |
| Poppler | [Project](https://poppler.freedesktop.org/), [source](https://gitlab.freedesktop.org/poppler/poppler), [text/layout API](https://poppler.freedesktop.org/api/cpp/classpoppler_1_1page.html) | Benchmark native utilities first; evaluate a small C++ adapter for sustained document sessions. Inspect the exact GPL license and notices before shipping. |
| PDFium | [Source/build](https://pdfium.googlesource.com/pdfium/+/HEAD/README.md), [text API](https://pdfium.googlesource.com/pdfium/+/main/public/fpdf_text.h), [license](https://pdfium.googlesource.com/pdfium/+/HEAD/LICENSE) | Own a pinned native build recipe. Start with V8/XFA disabled unless fixtures require them. |
| PDFium binding | [pdfium-render](https://github.com/ajrcarey/pdfium-render), [documentation](https://docs.rs/crate/pdfium-render/latest/source/README.md) | Pin the crate, explicit API feature, and native binary together. The wrapper does not supply/build PDFium; its mutex does not provide parallel PDFium throughput. |
| Bootstrap binaries | [PDFium binary publisher/build recipes](https://github.com/bblanchon/pdfium-binaries) | macOS/Linux arm64 artifacts are a possible initial baseline. Identify them as third-party builds, verify hashes/provenance, and measure source-build feasibility. |
| Docling Rust | [Official project](https://github.com/docling-project/docling.rs), [PDF conformance](https://github.com/docling-project/docling.rs/blob/master/docs/PDF_CONFORMANCE.md), [migration](https://github.com/docling-project/docling.rs/blob/master/docs/MIGRATION.md) | Use this native implementation rather than a similarly named HTTP client. Pin PDFium, ONNX Runtime, weights, dictionaries, preprocessing, and provider settings. |
| MLX | [MLX](https://github.com/ml-explore/mlx), [build support](https://ml-explore.github.io/mlx/build/html/install.html), [official C API](https://github.com/ml-explore/mlx-c), [community Rust binding](https://github.com/oxiglade/mlx-rs) | Test an existing Rust binding or narrow C boundary with one model before building a broad custom binding layer. |
| GPUI | [Framework](https://github.com/zed-industries/zed/tree/main/crates/gpui), [manifest/license](https://github.com/zed-industries/zed/blob/main/crates/gpui/Cargo.toml) | Later GUI-only dependency; pre-1.0 API changes require pinning. |
| Runners | [GitHub reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners) | Initial candidates: `macos-15` and `ubuntu-24.04-arm`, subject to SDK/runtime requirements. Record image/architecture and probe device support. |

## Constraints that change the design

**PDFium:** exposes Unicode, geometry, and mapping-error information. Some text APIs omit or cannot map characters; test non-BMP symbols and crop-box visibility explicitly. Preserve diagnostics. `pdfium-render` serializes native calls within a process, so bounded process-level concurrency is the baseline. Any alternative must establish the exact library's guarantees.

**Poppler:** positioned text and layout modes are useful comparisons, but its C++ documentation does not promise correct ordering for all scripts. GPL obligations differ from the MIT application scaffold and PDFium's BSD-style principal license. A subprocess boundary is not an automatic licensing exemption. Review exact source, third-party notices, and packaging before distributing linked or bundled components.

**Docling Rust:** the inspected PDF pipeline defaults to Rust `textparse.rs` on `lopdf`; `DOCLING_PDFIUM_TEXT=1` selects PDFium text fallback. PDFium remains a renderer. Its PDF conformance page reports 9/17 strict and 10/17 whitespace-normalized matches against Python Docling fixtures. These are upstream parity results, not independently checked human truth or this project's accuracy. General all-format compatibility claims must not become perfect-PDF claims.

The [CoreML section](https://github.com/docling-project/docling.rs/blob/master/README.md#gpu-execution-providers-optional-off-by-default) records incorrect boxes for some operations, numerical issues, and session startup costs. Its cited hardware measurements used M4 Max. Establish a CPU reference; measure M1 provider correctness, precision, setup, batches, and total throughput independently. Record actual provider selection and fallback. CoreML, Metal, and MLX are not interchangeable runtimes.

Docling Rust already exposes page selection and a streaming conversion callback. However, the inspected [PDF backend](https://github.com/docling-project/docling.rs/blob/908f080d5def736bbc094beeaf5b4a04fe504dd3/crates/docling-pdf/src/pdfium_backend.rs) opens parser state for each conversion: keeping model sessions warm does not keep the same document open across separate page-window conversions. Benchmark one streaming conversion per large document against repeated window calls. Preserve callback backpressure; treat resumable parser sessions and yielding a live document between jobs as capabilities to verify or implement, not existing guarantees.

The inspected [pipeline API](https://github.com/docling-project/docling.rs/blob/908f080d5def736bbc094beeaf5b4a04fe504dd3/crates/docling-pdf/src/lib.rs) also has `process_pages(Vec<PdfPage>)` for supplied page cells/images. Verify page-number preservation, outline/heading context, and coordinate compatibility before using it to avoid duplicate extraction. Streaming and buffered paths differ in confidence/heading handling at this revision; compare the requested structural fields, not just concatenated text, before treating streaming as equivalent.

**MLX:** current upstream supports Apple Metal and Linux CPU/CUDA configurations. This does not establish our Linux aarch64 binding/model packaging. ONNX models need a tested implementation/conversion for MLX, including operators and preprocessing; a Cargo feature cannot supply that conversion.

Reviewed research snapshots: Docling Rust `908f080d5def736bbc094beeaf5b4a04fe504dd3`; MLX `a32f0c3d46ecd4864cd7eae44552ed160d480dfb`; Zed/GPUI `4f70d91bda7ec5f5600a000a0dc34391d9f1e96f`. These identify reviewed state only. Reproduce claims at the selected implementation pins.

## Planned artifact lock

Add a small machine-readable native/model manifest alongside Cargo.lock. Record source URL/immutable revision, artifact URL/SHA256, target and deployment/glibc baseline, build-recipe digest, compiler/SDK/features, compatible binding/API, and license/notice locations. Models also need weight/dictionary/tokenizer hashes, preprocessing, precision, runtime/provider, and conversion-script identity.

Provision dependencies explicitly. Accepted artifacts must support offline processing; absent dependencies fail clearly. Contain upstream first-use download behavior so ordinary jobs cannot fetch moving libraries or alternate weights.

## Planned candidate-update workflow

Status 2026-09-30: only the detection half of step 1 is active. `.github/workflows/lifecycle-watch.yml` checks the PDFium binary releases and the Docling model release track daily (and by manual trigger) against `native/manifest.json`, applies a 7-day cooldown, and opens one issue per candidate; that issue, open or closed, is the cursor. Docling crates, `pdfium-render` and GPUI are Dependabot's (grouped, with cooldown). Poppler and MLX are not adopted, so nothing watches them. Steps 2 to 6 remain planned. Policy, runbooks and cost: [LIFECYCLE.md](LIFECYCLE.md).

1. Check releases and relevant heads on a daily schedule, plus manual trigger. Record a cursor; unchanged upstreams do not rebuild. Urgent candidates can be requested on demand.
2. Build changed candidates outside the release lane. Keep one update PR per coupled dependency group rather than per upstream commit. Measure PDFium's Chromium-tooling disk/time requirements before assuming a small runner can rebuild it.
3. Cache by full source/target/compiler/SDK/flags/recipe/dependency identity. Store accepted artifacts durably with provenance: disposable CI caches are not the release source of truth.
4. Run both native target checks, fixed-corpus quality comparisons, resource limits, model parity, and relevant large-document regression cases. Keep physical M1 performance evidence distinct from hosted timing noise.
5. Attach resolved pins, upstream changes, licenses, install results, quality diffs, and cost measurements to the PR. Initially no automatic merge. Failed candidates leave the accepted set usable.
6. Adoption creates a new processing identity; old results retain their original provenance. Rollback selects the previous manifest without overwriting evidence or rewriting Git history.

Prefer unmodified upstream releases. Keep required patches small, explicit, and covered by fixtures, with upstream issue/PR references. If a fork is needed, separate its tracking branch from accepted application pins. Do not continuously rebase whole upstream source trees into the application.

Apart from the detection above, these workflows are planned, not active yet. GitHub builds use public/synthetic fixtures. Bulk extraction runs on explicitly selected machines; do not make a personal Mac a public PR runner or upload private corpora as a side effect of implementation.
