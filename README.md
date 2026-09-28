# text-processing-engine

text-processing-engine

## Checks

Run these before opening a PR. CI runs the same ones.

```sh
uv run ruff format && uv run ruff check && uv run pytest && uv audit --preview-features audit-command
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
swift build && swift test
cmake -S . -B build && cmake --build build && ctest --test-dir build
```

`main` is protected: open a PR; the `ci` check must pass before merging (squash only).
