# Rust build contract

`rust-toolchain.toml` selects Rust **1.98.1** through rustup, with the minimal
profile plus the matching Clippy and rustfmt components. Run Cargo from the
repository or a descendant directory; a global `stable` default does not
override this file. Explicit `cargo +...`, `RUSTUP_TOOLCHAIN`, and rustup
directory overrides can override it and must not be used to publish builds
with a different compiler. `rustup show active-toolchain` and `rustc -Vv`
record what is actually selected.

The pin is the tested development and release compiler. The manifests do not
declare `rust-version`, and a lower minimum supported Rust version (MSRV) has
not been established. Edition 2024 and dependencies' declared Rust versions
are constraints, not evidence that the complete workspace works on their
oldest compiler. Establishing an MSRV requires its own locked build and test
matrix, including optional native features and the macOS app.

`Cargo.lock` is committed for the entire workspace. Production builds and CI
use `--locked`, so dependency resolution cannot silently replace it. Version
ranges in `Cargo.toml` express allowed upgrades; the lockfile selects exact
versions and registry checksums. Do not hand-edit package versions: release
versions come from git tags. Change dependencies deliberately and validate
the resulting lockfile with the pinned compiler before publishing.

`build.rs` embeds `TPE_VERSION` when supplied by the release workflow; the
conventional leading `v` is removed in `tpe --version`. An ordinary checkout
uses `git describe --tags --match 'v[0-9]*' --always --dirty`, so commits after
a tag and tracked local edits remain visible. A checkout without a matching
tag reports `git-<commit>`, and an archive without repository metadata reports
`source-archive`. Package manifest versions are not used as release identity.
Cargo watches the supplied environment value, git HEAD/index/refs (including
worktree metadata), and tracked source content so a reused build cannot retain
a stale tag or clean-tree identity. Release CI checks the actual executable's
version against its supplied tag.

```sh
rustup show active-toolchain
rustc -Vv
cargo -V
cargo clippy -V
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --release --locked -p text-processing-engine --bin tpe
```

`[profile.release]` in the root manifest controls the shipped code: fat LTO
and one codegen unit. Production runs use `target/release/tpe`, or
`cargo run --release --locked --bin tpe -- ...`; plain `cargo run` uses the
development profile. Add `--features pdfium` for the PDFium backend, and
provision the native library as described in [NATIVE.md](NATIVE.md). A native
library is an additional artifact outside Cargo.lock; record its hash too.

The macOS bundle script builds `tpe-app` with `--release --locked`. Linux
workspace tests cover engine-facing app code; they do not compile or launch
the macOS GUI. Its production build and bundle launch need the App workflow
on a native macOS runner. Python tools remain optional evaluation tooling,
managed only with uv, and are not needed to build or run the Rust engine.

For a reproducible run record the source commit and tree, dirty status,
toolchain versions and host target, Cargo.lock SHA-256, feature set, release
profile, build command, native artifact hashes, executable SHA-256, and the
input and output hashes. Those identities describe the observed build;
compiler and lockfile pinning alone do not guarantee byte-identical binaries
across different operating systems, linkers, or native libraries.
