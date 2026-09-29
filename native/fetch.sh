#!/bin/sh
# Provision the native artifacts pinned in native/manifest.json: the PDFium
# shared library for this platform (from bblanchon/pdfium-binaries,
# chromium/8066) into .pdfium/lib/, and the docling.rs models-v1 files the
# `docling` backend needs into .models/. See docs/NATIVE.md.
#
#   sh native/fetch.sh                 fetch what is missing, verify, print a table
#   sh native/fetch.sh --force         re-download everything
#   sh native/fetch.sh --print-hashes  no network: print file, bytes, sha256 of what is on disk
#
# Idempotent: a file already at its destination is not downloaded again, but
# it is always re-hashed and checked against the manifest. A manifest hash of
# null means "not pinned yet": the digest is printed (and the run passes) so it
# can be committed. A pinned hash that does not match fails the run.
#
# Every artifact has exactly one URL; there are no fallback mirrors, because a
# mutable fallback would defeat the pin. A missing release asset fails loudly.
set -eu

usage() {
  echo "usage: sh native/fetch.sh [--force] [--print-hashes]" >&2
}

FORCE=false
PRINT_ONLY=false
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=true ;;
    --print-hashes) PRINT_ONLY=true ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

# Work from the repository root whatever the caller's directory: docling and
# pdfium resolve `.models/` and `.pdfium/lib` relative to the process's CWD.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
MANIFEST=native/manifest.json
if [ ! -f "$MANIFEST" ]; then
  echo "error: $MANIFEST not found under $ROOT" >&2
  exit 1
fi

case "$(uname -s)" in
  Darwin) OS=mac ;;
  Linux) OS=linux ;;
  *) OS=unknown ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) ARCH=x64 ;;
  aarch64 | arm64) ARCH=arm64 ;;
  *) ARCH=unknown ;;
esac
PLATFORM="$OS-$ARCH"

sha256_of() { # <file> -> hex digest on stdout
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    echo "error: neither sha256sum nor shasum is available" >&2
    exit 1
  fi
}

bytes_of() { # <file> -> size in bytes (macOS wc pads with spaces)
  wc -c <"$1" | tr -d ' '
}

WORK=$(mktemp -d "${TMPDIR:-/tmp}/tpe-native.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
trap 'exit 130' INT TERM
ENTRIES="$WORK/entries"
ROWS="$WORK/rows"
: >"$ROWS"

# Flatten the manifest to one `|`-separated record per entry:
#   kind|platform|url|member|dest|archive_sha256|sha256
# with `-` for null. (`|` rather than a tab: read collapses runs of IFS
# whitespace, which would shift fields when one is empty.)
if command -v python3 >/dev/null 2>&1; then
  python3 - "$MANIFEST" >"$ENTRIES" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as f:
    manifest = json.load(f)
keys = ("kind", "platform", "url", "member", "dest", "archive_sha256", "sha256")
for entry in manifest["entries"]:
    values = [entry.get(k) or "-" for k in keys]
    for v in values:
        if "|" in v:
            sys.exit(f"manifest value contains '|': {v}")
    print("|".join(values))
PY
else
  # Line-oriented fallback: the manifest keeps each entry object on one line.
  field() { # <line> <key> -> string value, or `-` for null/missing
    v=$(printf '%s\n' "$1" | sed -n "s/.*\"$2\": *\"\\([^\"]*\\)\".*/\\1/p")
    if [ -n "$v" ]; then printf '%s' "$v"; else printf '%s' "-"; fi
  }
  grep '"kind"' "$MANIFEST" | while IFS= read -r line; do
    printf '%s|%s|%s|%s|%s|%s|%s\n' \
      "$(field "$line" kind)" "$(field "$line" platform)" "$(field "$line" url)" \
      "$(field "$line" member)" "$(field "$line" dest)" \
      "$(field "$line" archive_sha256)" "$(field "$line" sha256)"
  done >"$ENTRIES"
fi

if ! grep -q "^pdfium|$PLATFORM|" "$ENTRIES"; then
  echo "error: no pdfium entry for platform $PLATFORM in $MANIFEST" >&2
  exit 1
fi
if [ "$PRINT_ONLY" = false ] && ! command -v curl >/dev/null 2>&1; then
  echo "error: curl is required" >&2
  exit 1
fi

# Same guards as docling.rs's download_dependencies.sh: cap the connect phase,
# abort a stalled transfer, retry transient failures (a 404 still fails at once).
CURL_FLAGS="-fsSL --connect-timeout 30 --speed-limit 1024 --speed-time 60 --retry 8"

download() { # <url> <out>
  echo "  > $1" >&2
  # shellcheck disable=SC2086 # CURL_FLAGS is a flag list, splitting intended
  if ! curl $CURL_FLAGS -o "$2" "$1"; then
    rm -f "$2"
    echo "error: download failed: $1" >&2
    echo "       (is the asset still published at that exact URL?)" >&2
    return 1
  fi
}

FAILED=0
UNPINNED=0
while IFS='|' read -r kind platform url member dest archive_pin pin; do
  if [ "$platform" != any ] && [ "$platform" != "$PLATFORM" ]; then
    continue
  fi
  state=cached
  archive_hash=-
  if [ "$PRINT_ONLY" = true ]; then
    if [ ! -f "$dest" ]; then
      printf '%s|%s|%s|%s|%s\n' missing "$dest" - - - >>"$ROWS"
      continue
    fi
    state=present
  elif [ "$FORCE" = true ] || [ ! -f "$dest" ]; then
    mkdir -p "$(dirname "$dest")"
    if [ "$kind" = pdfium ]; then
      archive="$WORK/pdfium.tgz"
      if ! download "$url" "$archive"; then
        FAILED=1
        printf '%s|%s|%s|%s|%s\n' FAILED "$dest" - - - >>"$ROWS"
        continue
      fi
      archive_hash=$(sha256_of "$archive")
      if [ "$archive_pin" != - ] && [ "$archive_pin" != "$archive_hash" ]; then
        echo "error: $url: archive sha256 $archive_hash, manifest pins $archive_pin" >&2
        FAILED=1
        printf '%s|%s|%s|%s|%s\n' MISMATCH "$dest" - - "$archive_hash" >>"$ROWS"
        rm -f "$archive"
        continue
      fi
      rm -rf "$WORK/x"
      mkdir -p "$WORK/x"
      tar --no-same-owner -xzf "$archive" -C "$WORK/x" "$member"
      mv "$WORK/x/$member" "$dest"
      rm -f "$archive"
    else
      if ! download "$url" "$dest.download"; then
        FAILED=1
        printf '%s|%s|%s|%s|%s\n' FAILED "$dest" - - - >>"$ROWS"
        continue
      fi
      mv "$dest.download" "$dest"
    fi
    state=downloaded
  fi

  hash=$(sha256_of "$dest")
  size=$(bytes_of "$dest")
  if [ "$pin" = - ]; then
    UNPINNED=1
    [ "$state" = present ] || state="$state,unpinned"
  elif [ "$pin" != "$hash" ]; then
    if [ "$PRINT_ONLY" = false ]; then
      echo "error: $dest: sha256 $hash, manifest pins $pin" >&2
      echo "       (delete it or rerun with --force if the file on disk is stale)" >&2
      FAILED=1
      # Never leave a freshly downloaded, unverified file where it would be used.
      case "$state" in downloaded*) rm -f "$dest" ;; esac
    fi
    state=MISMATCH
  else
    [ "$state" = present ] || state="$state,verified"
  fi
  printf '%s|%s|%s|%s|%s\n' "$state" "$dest" "$size" "$hash" "$archive_hash" >>"$ROWS"
done <"$ENTRIES"

echo
echo "native artifacts for $PLATFORM ($ROOT)"
printf '%-22s %-32s %11s  %-64s  %s\n' state file bytes sha256 archive_sha256
while IFS='|' read -r state dest size hash archive_hash; do
  printf '%-22s %-32s %11s  %-64s  %s\n' "$state" "$dest" "$size" "$hash" "$archive_hash"
done <"$ROWS"

if [ "$UNPINNED" = 1 ]; then
  echo
  echo "note: some entries have sha256 null in $MANIFEST; copy the digests above"
  echo "      into the manifest to pin them (archive_sha256 is shown only on download)."
  if [ "${TPE_ALLOW_UNPINNED:-0}" != 1 ]; then
    echo "error: unpinned native artifacts are refused (set TPE_ALLOW_UNPINNED=1 to bootstrap)" >&2
    FAILED=1
  fi
fi
if [ "$FAILED" != 0 ]; then
  echo "error: native provisioning failed" >&2
  exit 1
fi
