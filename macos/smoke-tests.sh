#!/bin/bash
set -euo pipefail
app=${1:?usage: smoke-tests.sh TPE.app}
fixture=$(mktemp -t tpe-open).pdf
printf '%%PDF-1.4\n%%%%EOF\n' >"$fixture"
rm -f /tmp/tpe-launch-smoke /tmp/tpe-open-smoke
open -n "$app" --args --smoke-test
for _ in {1..30}; do test -e /tmp/tpe-launch-smoke && break; sleep 1; done
test -e /tmp/tpe-launch-smoke
open -n -a "$app" "$fixture"
for _ in {1..30}; do test -e /tmp/tpe-open-smoke && break; sleep 1; done
test "$(cat /tmp/tpe-open-smoke)" = "$fixture"
pkill -x TPE || true
