#!/bin/sh
# The required aggregate fails closed, including when path detection fails.
set -eu
test "$#" -eq 5
changes=$1
repo=$2
rust=$3
evaluator_required=$4
evaluator=$5
test "$changes" = success
test "$repo" = success
test "$rust" = success
case "$evaluator_required:$evaluator" in
  true:success|false:skipped|false:success) ;;
  *) exit 1 ;;
esac
