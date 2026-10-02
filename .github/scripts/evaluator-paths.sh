#!/bin/sh
# Read newline-separated repository paths. Git's quoted exotic filenames
# conservatively match the broad Python suffix as well.
set -eu
required=false
while IFS= read -r path; do
  case "$path" in
    *.py|*.py\"|pyproject.toml|uv.lock|.python-version|\
    .github/workflows/ci.yml|.github/workflows/evaluation-tools.yml|\
    .github/workflows/eval.yml|.github/workflows/native.yml|\
    .github/workflows/pmc-bibliography.yml|.github/workflows/native-pmc200.yml|\
    .github/workflows/poppler-compare.yml|\
    .github/workflows/auto-release.yml|.github/scripts/*|.github/tests/*)
      required=true ;;
  esac
done
printf '%s\n' "$required"
