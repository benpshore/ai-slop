# text-processing-engine

- `main` is protected. Work on a branch and open a PR (`gh pr create --fill`); the `ci` check must pass.
- Never commit secrets, `.env` files, databases or `.DS_Store` (see `.gitignore`; CI rejects them).
- Python: use **uv** only (`uv add`, `uv run`). Never pip.
- The version is the git tag; never edit a version field by hand.
- Every merge to `main` is released automatically (next minor version, wheel and sdist attached).
- Before committing, run:

```sh
uv run ruff format && uv run ruff check && uv run pytest && uv audit --preview-features audit-command
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
swift build && swift test
cmake -S . -B build && cmake --build build && ctest --test-dir build
```
