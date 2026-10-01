# pdftextract

- `main` is protected. Work on a branch and open a PR (`gh pr create --fill`); the `ci` check must pass.
- Never commit secrets, `.env` files, databases or `.DS_Store` (see `.gitignore`; CI rejects them).
- The version is the git tag; never edit a version field by hand.
- Every merge to `main` is released automatically with native Rust binaries.
- Before committing Rust changes, run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Python is optional accuracy-evaluation tooling, not an application package. When
changing Python scripts, their tests, or their dependency configuration, use **uv**
only (never pip) and run:

```sh
uv sync --locked
uv run ruff format && uv run ruff check && uv run pytest
uv audit --locked --preview-features audit-command
```

Evaluation-tool checks run only for relevant paths; routine CI checks the repository
and Rust on x86-64 and ARM64 Linux. The removed Python package, Swift and CMake
scaffolds are preserved on `archive/python-swift-cmake-2026-10-01`.
