#!/bin/sh
# This opt-in build is never run by the normal build or release workflows.
set -eu
: "${MUPDF_INCLUDE_DIR:?set the absolute MuPDF include directory}"
: "${MUPDF_LIBRARY_PATH:?set the absolute separately licensed MuPDF shared library}"
: "${MUPDF_PROVIDER_OUT:?set the absolute output path for your private provider}"
case "$MUPDF_INCLUDE_DIR:$MUPDF_LIBRARY_PATH:$MUPDF_PROVIDER_OUT" in /*:/*:/*) ;; *) echo 'all MuPDF paths must be absolute' >&2; exit 1;; esac
source_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
lib_dir=$(dirname -- "$MUPDF_LIBRARY_PATH")
case $(uname -s) in
  Darwin) link_kind=-dynamiclib ;;
  Linux) link_kind=-shared ;;
  *) echo 'provider build currently supports Linux and macOS' >&2; exit 1 ;;
esac
"${CC:-cc}" -std=c11 -O2 -Wall -Wextra -Werror -fPIC "$link_kind" \
  -I "$MUPDF_INCLUDE_DIR" "$source_dir/provider.c" "$MUPDF_LIBRARY_PATH" \
  "-Wl,-rpath,$lib_dir" -o "$MUPDF_PROVIDER_OUT"
