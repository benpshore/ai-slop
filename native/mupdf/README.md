# MuPDF provider (optional user build)

MuPDF is available under AGPLv3+ or a commercial license. Obtain a license and
runtime suitable for your deployment; this repository does not relicense the
engine, distribute its binary, or assert that dynamic loading avoids license
obligations. Official sources:
https://mupdf.readthedocs.io/en/1.28.5/license.html and https://mupdf.com/releases.
The current upstream release checked is 1.28.5. The local integration prototype was
built and exercised against MuPDF **1.26.11**, not 1.28.5.

Use matching, separately obtained headers/shared library and a C11 compiler:

```sh
MUPDF_INCLUDE_DIR=/absolute/mupdf/include \
MUPDF_LIBRARY_PATH=/absolute/mupdf/libmupdf.so \
MUPDF_PROVIDER_OUT=/absolute/libtpe-mupdf-provider.so \
sh native/mupdf/build-provider.sh
cargo build --features mupdf
TPE_MUPDF_PROVIDER_PATH=/absolute/libtpe-mupdf-provider.so \
MUPDF_DYNAMIC_LIB_PATH=/absolute/mupdf/libmupdf.so \
./target/debug/tpe extract --backend mupdf --db /absolute/output.sqlite input.pdf
```

The opt-in script supports Linux/macOS, embeds an absolute runtime search directory,
and is never run by normal Cargo/release builds. Native bindings use a private
MuPDF context per document. All `fz_try`/`fz_catch` scopes remain in C; no MuPDF
longjmp crosses Rust. MuPDF's compile/runtime compatibility check runs when creating
the context. The 8 MiB MuPDF store is a cache preference, not a peak memory bound;
the CLI worker's existing limits contain allocations and malformed documents.

Structured text preserves spans, images, and whitespace. Unknown CID/GID fallback
is disabled; replacements and native parser warnings keep the page Partial. Link
annotation targets are read directly from raw PDF URI actions; rectangles use
MuPDF's page transform. Catalog URI bases and non-URI actions are not substituted.
URI action strings follow the PDF ASCII-string contract; invalid UTF-8 output is
rejected instead of silently replacing bytes. The shared ABI
contract and limits are described in [../README.md](../README.md).

Configured integration test:

```sh
TPE_MUPDF_PROVIDER_PATH=/absolute/libtpe-mupdf-provider.so \
MUPDF_DYNAMIC_LIB_PATH=/absolute/mupdf/libmupdf.so \
cargo test --features mupdf --test native_provider -- --ignored
```
