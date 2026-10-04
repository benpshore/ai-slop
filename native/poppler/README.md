# Optional Poppler provider

This adapter implements `native/provider.h` against **Poppler 26.09.0**, the
latest stable release listed by the [upstream project](https://poppler.freedesktop.org/)
on 2026-10-04. It uses Poppler's core C++ API through a narrow C ABI. All routing,
Unicode normalization, reading order, bibliography extraction, output schemas,
and publication remain in Rust. It launches no subprocesses.

Poppler is **GPL-2.0-or-later**, not LGPL. Its upstream README explicitly says
programs calling Poppler must be licensed under the GPL. This repository's
license does not change. An optional loader or user-supplied shared library does
not waive those obligations. Build and use this integration only under a
compatible license arrangement. No Poppler binary or source archive is bundled
in the default application or this directory.

## Build the actual runtime

Obtain the official stable source archive:

```
https://poppler.freedesktop.org/poppler-26.09.0.tar.xz
SHA256 8059eadb6805340768f138c465b57f8164c92b4a0773c37ef031ea6c0d987b2e
```

The build needs a C++23 compiler, CMake >= 3.28, pkg-config, and development
packages for FreeType >= 2.13, Fontconfig >= 2.15 (Linux), HarfBuzz, zlib, PNG,
JPEG, and OpenJPEG. Install upstream `poppler-data` 0.4.12 for nonembedded CJK
mapping data; its location and system font availability can affect extraction.
This integration does not download dependencies or silently select an older
installed Poppler. The core API is unstable, so rebuild the provider with the
exact same release as its runtime. A compile-time version check rejects other
headers.

Example Linux build (choose absolute source, build, and install directories):

```sh
cmake -S "$POPPLER_SOURCE" -B "$POPPLER_BUILD" \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$POPPLER_PREFIX" \
  -DENABLE_UNSTABLE_API_ABI_HEADERS=ON \
  -DENABLE_CPP=OFF -DENABLE_GLIB=OFF -DENABLE_QT5=OFF -DENABLE_QT6=OFF \
  -DENABLE_UTILS=OFF -DENABLE_BOOST=OFF -DENABLE_LCMS=OFF \
  -DENABLE_LIBCURL=OFF -DENABLE_LIBTIFF=OFF -DENABLE_NSS3=OFF -DENABLE_GPGME=OFF \
  -DBUILD_GTK_TESTS=OFF -DBUILD_QT5_TESTS=OFF -DBUILD_QT6_TESTS=OFF \
  -DBUILD_CPP_TESTS=OFF -DBUILD_MANUAL_TESTS=OFF
cmake --build "$POPPLER_BUILD" --parallel 2
cmake --install "$POPPLER_BUILD"
```

JPEG/JPEG2000 decoding, fonts, and mapping support stay enabled. Optional signing,
network retrieval, rendering wrappers, and image export are not required for this
text adapter. Poppler's ordinary password decryption remains available without
the optional signature backends.

## Build and select the provider

After the shared header and optional Rust `poppler` feature are present:

```sh
cmake -S native/poppler -B "$PROVIDER_BUILD" \
  -DPOPPLER_INCLUDE_DIR="$POPPLER_PREFIX/include/poppler" \
  -DPOPPLER_LIBRARY="$POPPLER_PREFIX/lib/libpoppler.so.164.0.0"
cmake --build "$PROVIDER_BUILD" --parallel 2
ctest --test-dir "$PROVIDER_BUILD" --output-on-failure
cargo build --locked --release --features poppler
export TPE_POPPLER_PROVIDER_PATH="$PROVIDER_BUILD/tpe-poppler-provider.so"
export POPPLER_DYNAMIC_LIB_PATH="$POPPLER_PREFIX/lib/libpoppler.so.164.0.0"
```

Use the platform's actual shared-library filename on other systems. The Linux
runtime and provider have been compiled and exercised against the exact source
above; additional platforms require their own build and runtime validation.
Both environment variables must be absolute file paths. The Rust loader hashes
the selected files, records that identity, and checks that the provider's
`globalParams` data-symbol address belongs to the selected core runtime. It does
not load a runtime from a PDF, current directory, or executable search path.
Native system dependencies and font/mapping files are deployment dependencies;
their identities are not included in the two shared-library hashes.

## Data and failure contract

The provider borrows immutable PDF bytes until `close`, accepts a password, and
returns one page of ABI-v1 JSON. It preserves native word boxes, Unicode text,
font names and sizes, bold/italic flags, and URI link annotations. The visible
label stays in text spans; the raw annotation URI remains separate. Catalog base
URLs do not rewrite it, and a `www.` prefix does not acquire an invented scheme. Annotation
rectangles use the same crop and rotation transform as text. Only URI actions
become external links; JavaScript and launch actions are never executed.

Every word is a positioned font run. JSON coordinates are top-left page points;
the Rust adapter converts them to the common page coordinate system. Native
warnings, reconstructed cross-references, and missing Unicode mappings produce
explicit incomplete-extraction evidence. They are not silently reported as a
complete page. Empty valid pages remain empty without a fabricated warning.

JSON construction is bounded to the caller's limit and at most 16 MiB, with at
most one million emitted Unicode scalars and 100,000 URI links per page. Failures
return null/zero output, a bounded error, and no partially owned buffer. Exceptions
never cross the C ABI. Poppler calls are serialized because its global error
callback and configuration are shared; warning counts remain document-local.
Native parse allocations are contained by the existing CLI worker's OS memory
limit and deadline. Direct library callers must supply their own process boundary.

`test_provider.py` tests the actual runtime identity, text/font geometry, distinct
DOI annotation target, unmodified relative links, rotated and cropped coordinates,
malformed content and missing-character evidence, required/wrong/correct passwords,
bad input, output refusal, and successful reuse after a failed page
request. It needs only Python's standard library. It is not a simulated engine.
