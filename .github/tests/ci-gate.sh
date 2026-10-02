#!/bin/sh
# Exercise the actual scripts called by CI; no Python needed on Rust changes.
set -eu
cd "$(dirname "$0")/../.."
check=.github/scripts/check-ci.sh
paths=.github/scripts/evaluator-paths.sh
sh "$check" success success success true success
sh "$check" success success success false skipped
for result in failure cancelled skipped ''; do
  if sh "$check" success success success true "$result"; then
    echo "required evaluator result '$result' incorrectly passed" >&2
    exit 1
  fi
done
for result in failure cancelled skipped ''; do
  if sh "$check" "$result" success success false skipped; then
    echo "failed path detection incorrectly passed" >&2
    exit 1
  fi
done
if sh "$check" success success success '' skipped; then exit 1; fi
if sh "$check" success success success false failure; then exit 1; fi
if sh "$check" success failure success false skipped; then exit 1; fi
if sh "$check" success success cancelled false skipped; then exit 1; fi
for path in scripts/pmc_bib_eval.py native/validate_eval.py tests/test_smoke.py \
  pyproject.toml uv.lock .python-version .github/workflows/ci.yml \
  .github/workflows/evaluation-tools.yml .github/workflows/pmc-bibliography.yml \
  .github/workflows/native-pmc200.yml \
  .github/scripts/check-ci.sh; do
  test "$(printf '%s\n' "$path" | sh "$paths")" = true
done
test "$(printf '%s\n' src/main.rs Cargo.toml Cargo.lock README.md | sh "$paths")" = false
test "$(printf '' | sh "$paths")" = false
