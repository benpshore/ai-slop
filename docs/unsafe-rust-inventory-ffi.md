# Unsafe Rust and FFI inventory

This inventory records the Rust `unsafe` syntax in the production source snapshot `68ee9b4e08fde2f28e1042e1d6abb89e3547770d` (the frozen local integration source), which produced the versioned release binary. This source snapshot extends the toolchain/release work in [PR #140](https://github.com/benpshore/pdftextract/pull/140); inventory tracking is in [issue #145](https://github.com/benpshore/pdftextract/issues/145). It finds **35 source occurrences** across 95 tracked Rust files: 34 unsafe blocks and one unsafe function. The scan found no unsafe extern blocks or functions, unsafe traits or impls, Rust 2024 `#[unsafe(...)]` attributes, or additional unsafe forms in repository macro token bodies. Nine are in the macOS AppKit services module and 26 are in the macOS AVFAudio speech module.

The audit includes each source path and line, the containing symbol, the concrete operation, nearby safety notes, visible invariants or gaps, the source file SHA-256, and the audited commit. The source manifest lists all tracked `.rs` file paths and the SHA-256 of each exact file. Its deterministic format and digest are recorded in `unsafe-rust-inventory-ffi/summary.json`.

The findings include code behind macOS platform configuration. The production build was Linux x86-64 with the `pdfium` feature, so it does not demonstrate compilation or runtime behavior for the macOS FFI modules. The scan covers repository Rust source only; it excludes Cargo dependencies, generated build output, and vendored code. It describes visible source contracts and documentation. It does not prove the safety of an FFI call or any dependency.

The successful release evidence is preserved in `unsafe-rust-inventory-ffi/build-manifest.json` (manifest SHA-256 `496c95f2b39975089cd440a6c76ba1f503051009d00b75dfd591a06571dc5f82`). It records the exact source commit, clean source/lock state, toolchain, command, target, binary identity, and successful PDFium and automatic-backend runs. The actual binary reports `tpe git-68ee9b4e08fde2f28e1042e1d6abb89e3547770d`.

The JSONL findings are an inventory, not a risk ranking or a claim that every entry is defective. Several FFI APIs are unsafe because Rust cannot verify Objective-C receiver, selector, ownership, pointer, or lifetime contracts. The entries distinguish preconditions stated in source from those left implicit. Inventory maintenance is tracked in [issue #145](https://github.com/benpshore/pdftextract/issues/145); the related dependency soundness report is [issue #122](https://github.com/benpshore/pdftextract/issues/122).

## Artifacts

- [Findings JSONL](unsafe-rust-inventory-ffi/findings.jsonl)
- [Summary and hashes](unsafe-rust-inventory-ffi/summary.json)
- [Tracked Rust source manifest](unsafe-rust-inventory-ffi/source-manifest.jsonl)
- [Production build manifest](unsafe-rust-inventory-ffi/build-manifest.json)
- [Build source hash manifest](unsafe-rust-inventory-ffi/build-source-sha256.json)
