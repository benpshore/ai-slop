# Dependency maintenance

Dependencies stay pinned and updates arrive as reviewable pull requests.
The `Dependency updates` workflow checks daily at 06:23 UTC; GitHub may delay
scheduled runs. It runs on the default branch after this workflow is merged.
Nothing here enables auto-merge or changes the branch ruleset.

| Dependency | Update mechanism | Bound |
| --- | --- | --- |
| Docling Rust suite | Dedicated `docling` update track | One open PR; coordinated stable version across `docling`, `docling-core`, `docling-pdf`, `docling-onnx`, `docling-asr` |
| PDFium binary | Dedicated `pdfium` update track | One open PR; only the three platforms already in the native manifest |
| OCR/layout model assets | Dedicated `models` update track | One open PR; only existing files in the separately pinned `models_release` |
| Other Cargo dependencies | Daily Dependabot | Up to three open update PRs |
| GitHub Actions, Python/uv, Swift packages | Daily Dependabot | Up to two open PRs per ecosystem |

The custom tracks run sequentially with a 20-minute limit each. An open
proposal is left unchanged so reviewer edits and evidence are preserved;
newer releases wait behind it. There are at most three custom update PRs.
Close a rejected proposal only after deciding whether to defer its release,
since the next daily run will propose the same still-current version again.
Failures remain visible as failed workflow runs, not successful no-change checks.

## Reproducibility and checksums

`native/fetch.sh` still consumes exact URLs and hashes from
`native/manifest.json`. It never discovers a moving `latest` at runtime.
The maintenance script checks the upstream release API before proposing:

- PDFium: a stable `chromium/<build>` release, with every existing platform
  asset present. Its archive must match GitHub's SHA-256 and declared byte
  count. The selected regular library member gets an independently computed
  installed-file SHA-256. Duplicate, linked, missing or oversized members fail.
- Models: the current assets in the explicitly selected model release track.
  Replacing an asset under `models-v1` changes its checksum and produces a PR.
  A new model family/tag needs an explicit compatibility decision; it is not
  inferred from a Docling crate or npm version.
- Docling: the newest common stable, non-yanked version published for all five
  Rust crates, confirmed by its exact `v<version>` GitHub release. GitHub's
  `/releases/latest` can point to an npm CUDA binary release and is not used
  to select the Rust version. Cargo retains registry checksums in its lockfile.

Changed artifacts are downloaded to memory only: at most 128 MiB per asset,
128 MiB for the selected library and 256 MiB total archive expansion, with
10,000 archive members maximum. Unchanged asset hashes avoid downloads.
The updater does not load libraries, execute model files, or fetch optional
models absent from the manifest. Provisioning uses `--no-same-owner`, supported
by GNU and BSD tar, so archive user IDs do not require container privileges.

## Version identities and compatibility

`uv run python scripts/refresh_dependency_versions.py` refreshes the known
extractor identity constants from the root package's direct Cargo.lock entries
and the PDFium manifest release. It distinguishes the engine's direct lopdf
version from Docling's separate lopdf version. It also updates the PDFium
identity fixture and the Docling/CSV/Calamine ingestion identities.
The repository test suite rejects stale identities.

Custom update tracks call the refresher automatically. A Dependabot Cargo PR
that changes one of these extractor versions needs the same command committed
as a follow-up before CI can pass. Other dependency updates need no identity
edit. This deliberate failure prevents shipping old provenance labels. The
refresher is an update-time tool, not a Cargo build hook: a library's bundled
lockfile cannot establish a downstream consumer's actual dependency resolution.

As verified on 2026-09-29, the coordinated Docling suite is **1.74.1**,
PDFium's latest published binary release is **chromium/8066**, and the four
pinned `models-v1` assets match upstream digests. `pdfium-render` **0.9.4** is
newer than our **0.8.37**, but Docling 1.74.1 still requires the **0.8** line.
Its update is explicitly held for a coordinated binding migration; loading two
independent PDFium binding versions needs integration review. This is a known
compatibility exception, not a claim that every dependency is latest.

The Swift CI container tag/digest and ONNX Runtime's native distribution are
separate provisioning dependencies. Dependabot's GitHub Actions support does
not currently update job-container images. They require a reviewed provisioning
update; this PR does not claim automated coverage for those artifacts.

## Validation and GitHub operation

Generated PRs explicitly dispatch `ci.yml` and `native.yml` on their head
branch. Current GitHub behavior can put `GITHUB_TOKEN`-created PR event runs
in an approval-required state; `workflow_dispatch` runs directly. A later
maintenance run dispatches checks only when that workflow has no dispatch for
the PR's current commit, recovering a failed dispatch without rerunning completed
checks daily. A failed actual test run remains failed for investigation.

The repository must allow GitHub Actions to create pull requests, and the
workflow requests only the contents/PR/actions permissions used for publication
and check dispatch. Missing permission fails visibly. It never approves a PR,
resolves review threads, or enables auto-merge. Existing required `ci` and
review-thread gates continue to apply; Native checks remain separately visible.

Native validation includes `docling,pdfium,formats` builds and fixture tests.
Corpus download/evaluation is now **opt-in**, via Native's `run_eval: true`
manual-dispatch input. Routine dependency PRs and pushes do not rerun it.
Fixture success verifies integration contracts, not overall OCR or bibliography
accuracy, and does not establish performance on a library collection.

Local preparation, which changes files but does not publish:

```sh
uv run python scripts/update_dependencies.py --track docling --report /tmp/docling-update.md
uv run python scripts/update_dependencies.py --track pdfium --report /tmp/pdfium-update.md
uv run python scripts/update_dependencies.py --track models --report /tmp/models-update.md
uv run python scripts/refresh_dependency_versions.py --check
uv run pytest tests/test_dependency_updates.py
```

Primary sources checked for this change:

- [Docling Rust 1.74.1 release](https://github.com/docling-project/docling.rs/releases/tag/v1.74.1)
- [Published Docling PDF crate](https://crates.io/crates/docling-pdf/1.74.1)
- [PDFium chromium/8066 release](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F8066)
- [Separate models-v1 assets](https://github.com/docling-project/docling.rs/releases/tag/models-v1)
- [GitHub workflow-trigger behavior](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow)
- [Dependabot ecosystem limits](https://docs.github.com/en/code-security/reference/supply-chain-security/supported-ecosystems-and-repositories)
- [Job-container image update request](https://github.com/dependabot/dependabot-core/issues/5819)
