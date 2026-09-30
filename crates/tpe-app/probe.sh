#!/bin/sh
# Measure PDFTextract's responsiveness in a REAL session, with the app's own
# probe (docs/APP.md, "Measuring the real app"). macOS only.
#
#   sh crates/tpe-app/probe.sh                        # builds the app, runs everything
#   sh crates/tpe-app/probe.sh --app /path/PDFTextract.app --out ~/Desktop/probe
#
# It launches the app with the probe on (PDFTEXTRACT_PROBE, off otherwise),
# lets the app's scripted driver press keys and scroll the list, feeds it PDFs
# through Launch Services (`open -a`: the Open With / Dock-drop path) in
# batches while it works, quits it, and leaves the report in the output
# directory. Send back report.json and report.txt (and environment.txt).
#
# Options:
#   --app PATH        a built PDFTextract.app (default: build one with bundle.sh)
#   --out DIR         where the report goes (default: ./pdftextract-probe-<time>)
#   --counts "1 50 500 5000"   batch sizes of 2-page PDFs to open at once
#   --pages N         pages of the one long PDF (default 2000)
#   --papers N        20-page PDFs opened at once (default 500)
#   --screenshot      also capture the screen once, mid-run (CI does)
#   --hands-off       do not warn about touching the machine while it runs
#
# While it runs, leave the machine alone: the app is real, so anything else
# you do competes with it. Do not run it on battery saver.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
APP=""
OUT=""
COUNTS="1 50 500 5000"
PAGES=2000
PAPERS=500
SHOT=0
while [ $# -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --counts) COUNTS=$(echo "$2" | tr ',' ' '); shift 2 ;;
    --pages) PAGES=$2; shift 2 ;;
    --papers) PAPERS=$2; shift 2 ;;
    --screenshot) SHOT=1; shift ;;
    --hands-off) shift ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

[ "$(uname -s)" = Darwin ] || { echo "probe.sh needs macOS" >&2; exit 2; }
if [ -z "$APP" ]; then
  sh "$HERE/bundle.sh"
  APP=${CARGO_TARGET_DIR:-$ROOT/target}/release/PDFTextract.app
fi
BIN="$APP/Contents/MacOS/PDFTextract"
[ -x "$BIN" ] || { echo "no app at $APP" >&2; exit 2; }
[ -n "$OUT" ] || OUT="$PWD/pdftextract-probe-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/pdftextract-probe.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

log() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/driver.log"; }

# --- the machine, so the numbers can be read against it ---------------------
{
  echo "date: $(date)"
  echo "macOS: $(sw_vers -productName) $(sw_vers -productVersion) ($(sw_vers -buildVersion)), $(uname -m)"
  echo "cpu: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)"
  echo "model: $(sysctl -n hw.model 2>/dev/null || echo unknown)"
  echo "cores: $(sysctl -n hw.ncpu 2>/dev/null) logical; performance $(sysctl -n hw.perflevel0.logicalcpu 2>/dev/null || echo n/a), efficiency $(sysctl -n hw.perflevel1.logicalcpu 2>/dev/null || echo n/a)"
  echo "memory bytes: $(sysctl -n hw.memsize 2>/dev/null || echo unknown)"
  echo "--- power"
  pmset -g batt 2>/dev/null | head -3 || true
  pmset -g 2>/dev/null | grep -iE 'lowpowermode|powermode|sleep' || true
  pmset -g therm 2>/dev/null | head -5 || true
  echo "--- displays"
  system_profiler SPDisplaysDataType 2>/dev/null | grep -E 'Chipset|Type:|Resolution|Refresh|Metal|Vendor|Display Type|Main Display|Mirror' || true
  echo "--- load before the run"
  uptime
} > "$OUT/environment.txt" 2>&1 || true
NOTE="$(sysctl -n machdep.cpu.brand_string 2>/dev/null), $(sysctl -n hw.model 2>/dev/null), macOS $(sw_vers -productVersion), cores $(sysctl -n hw.perflevel0.logicalcpu 2>/dev/null || echo ?)P+$(sysctl -n hw.perflevel1.logicalcpu 2>/dev/null || echo ?)E, $(pmset -g batt 2>/dev/null | head -1 | sed 's/^Now drawing from //' || echo power unknown)"

# --- the PDFs ----------------------------------------------------------------
"$BIN" --probe-make-pdf 2 "$WORK/small.pdf" >/dev/null
"$BIN" --probe-make-pdf 20 "$WORK/paper.pdf" >/dev/null
"$BIN" --probe-make-pdf "$PAGES" "$WORK/long.pdf" >/dev/null

copies() { # copies SOURCE DIR N
  mkdir -p "$2"
  i=1
  while [ "$i" -le "$3" ]; do cp "$1" "$2/p$i.pdf"; i=$((i + 1)); done
}
opens() { # opens DIR: one Launch Services open of every PDF in it
  find "$1" -name '*.pdf' -print0 | xargs -0 -s 900000 open -a "$APP"
}
outputs() { find "$1" -name '*.txt' 2>/dev/null | wc -l | tr -d ' '; }
wait_for() { # wait_for DIR N SECONDS
  waited=0
  while [ "$(outputs "$1")" -lt "$2" ] && [ "$waited" -lt "$3" ]; do
    sleep 1
    waited=$((waited + 1))
  done
  log "  $(outputs "$1") of $2 done after ${waited}s"
}

# --- launch the app with the probe on ---------------------------------------
# The app gets its own empty HOME so the run's thousands of synthetic jobs and
# its text-size changes never touch the real ledger or settings. (Fonts, GPU
# caches and Launch Services are the system's and are unaffected.)
mkdir -p "$WORK/home"
log "launching $APP with the probe on (private HOME, empty ledger)"
HOME="$WORK/home" PDFTEXTRACT_PROBE="$OUT/report.json" PDFTEXTRACT_PROBE_DRIVE=1 \
  PDFTEXTRACT_PROBE_NOTE="$NOTE" ZED_MEASUREMENTS=1 \
  "$BIN" 2> "$OUT/stderr.log" &
PID=$!
sleep 8
kill -0 "$PID" 2>/dev/null || { echo "the app did not start; see $OUT/stderr.log" >&2; exit 1; }
log "idle baseline (the driver presses keys and toggles the text size)"
sleep 6

# One PDF, then batches, then one long document, then many papers.
for n in $COUNTS; do
  d="$WORK/batch-$n"
  copies "$WORK/small.pdf" "$d" "$n"
  log "opening $n two-page PDFs in one Launch Services call"
  opens "$d"
  if [ "$SHOT" = 1 ] && [ "$n" = 500 ]; then
    sleep 3
    screencapture -x "$OUT/window-mid-run.png" || log "screencapture failed"
  fi
  wait_for "$d" "$n" $((120 + n / 4))
  sleep 3
done
log "opening one $PAGES-page PDF"
mkdir -p "$WORK/long"
cp "$WORK/long.pdf" "$WORK/long/long.pdf"
opens "$WORK/long"
wait_for "$WORK/long" 1 600
sleep 3
log "opening $PAPERS twenty-page PDFs in one Launch Services call"
copies "$WORK/paper.pdf" "$WORK/papers" "$PAPERS"
opens "$WORK/papers"
wait_for "$WORK/papers" "$PAPERS" $((120 + PAPERS))
log "idle again"
sleep 8

# --- stop it and collect the report -----------------------------------------
[ "$SHOT" = 1 ] && { screencapture -x "$OUT/window-end.png" || true; }
log "asking the app to quit (it writes the report on the way out)"
osascript -e 'tell application "PDFTextract" to quit' >/dev/null 2>&1 || kill -TERM "$PID" 2>/dev/null || true
waited=0
while kill -0 "$PID" 2>/dev/null && [ "$waited" -lt 30 ]; do sleep 1; waited=$((waited + 1)); done
if kill -0 "$PID" 2>/dev/null; then
  log "the app did not quit; killing it (the report is its last periodic snapshot)"
  kill -9 "$PID" 2>/dev/null || true
fi
wait "$PID" 2>/dev/null || true

if [ -f "$OUT/report.json" ]; then
  "$BIN" --probe-merge "$OUT/stderr.log" "$OUT/report.json" || log "merging GPUI's frame timings failed"
  echo
  cat "$OUT/report.txt"
  echo
  log "report: $OUT/report.json and $OUT/report.txt (send these, and environment.txt)"
else
  log "NO REPORT was written; see $OUT/stderr.log"
  exit 1
fi
